use clap::{Parser, Subcommand};
use rgraph::db::database::Database;
use rgraph::io::posix::PosixFileSystem;
use rgraph::server::{
    AsyncGraphEngine, ConnectionAcceptor, GraphEngineAdapter, GraphGrpcServer, MetricsCollector,
    RequestDispatcher, ServerConfig, ServerRuntime,
};
use rgraph::storage::page::{PageType, SlottedPage};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use tracing::info;
use rgraph::cypher::executor::execute_expression_query;
use rgraph::cypher::parser::parse;
use rgraph::cypher::semantic::analyse;
use rgraph::cypher::planner::plan;
use rgraph::cypher::physical::{execute_plan, ExecutionContext};
use rgraph::graph::engine::GraphStorageEngine;

#[derive(Parser)]
#[command(name = "rgraph")]
#[command(about = "RGraph - A high-performance graph database engine")]
struct Cli {
    /// Increase logging verbosity (repeat for more detail).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
    /// Suppress all output except errors.
    #[arg(short, long)]
    quiet: bool,
    /// Emit structured JSON log lines.
    #[arg(long)]
    json_log: bool,
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new empty database.
    Init { path: PathBuf },
    /// Open an existing database and show meta info.
    Open { path: PathBuf },
    /// Insert a dummy record into a slotted page and commit.
    Insert { path: PathBuf, data: String },
    /// Read back a record from a slotted page.
    Read {
        path: PathBuf,
        page_id: u64,
        slot: u16,
    },
    /// Start the server in async mode.
    Serve {
        /// Host to bind (default: 0.0.0.0).
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
        /// Port to bind (default: 7687).
        #[arg(long, default_value_t = 7687)]
        port: u16,
        /// Number of Tokio worker threads.
        #[arg(long, default_value_t = 0)]
        workers: usize,
        /// Maximum concurrent connections.
        #[arg(long, default_value_t = 1024)]
        max_connections: usize,
        /// Number of CPU-bound rayon threads.
        #[arg(long, default_value_t = 0)]
        cpu_threads: usize,
        /// Path to database directory.
        path: PathBuf,
        /// Enable TLS.
        #[arg(long)]
        tls: bool,
        /// Path to TLS certificate.
        #[arg(long, requires = "tls")]
        tls_cert: Option<PathBuf>,
        /// Path to TLS private key.
        #[arg(long, requires = "tls")]
        tls_key: Option<PathBuf>,
    },
    /// Run a Cypher query against the database (or in expression-only mode).
    Query {
        /// Path to database directory.
        path: PathBuf,
        /// The Cypher query string to execute.
        query: String,
        /// Emit results as JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Import data from CSV, JSONL, or Turtle.
    Import {
        /// Path to database directory.
        path: PathBuf,
        /// Input file to import.
        #[arg(short, long)]
        file: PathBuf,
        /// Format: csv, jsonl, turtle.
        #[arg(short, long, default_value = "csv")]
        format: String,
    },
    /// Export data to Cypher or Turtle.
    Export {
        /// Path to database directory.
        path: PathBuf,
        /// Output file.
        #[arg(short, long)]
        output: PathBuf,
        /// Format: cypher, turtle.
        #[arg(short, long, default_value = "cypher")]
        format: String,
        /// Optional label filter.
        #[arg(long)]
        label: Option<String>,
    },
    /// Run a benchmark workload.
    Benchmark {
        /// Path to database directory.
        path: PathBuf,
        /// Number of iterations.
        #[arg(short, long, default_value_t = 1000)]
        iterations: usize,
        /// Concurrency level.
        #[arg(short, long, default_value_t = 10)]
        concurrency: usize,
    },
}

/// Map an [`RGraphError`] to a distinct CLI exit code.
fn exit_code_for(err: &rgraph::error::RGraphError) -> i32 {
    use rgraph::error::RGraphError;
    match err {
        RGraphError::Syntax(_) => 2,
        RGraphError::Semantic(_) => 3,
        RGraphError::Type(_) => 4,
        RGraphError::Argument(_) => 5,
        RGraphError::NotFound(_) => 6,
        RGraphError::AlreadyExists(_) => 7,
        RGraphError::Io(_) => 8,
        RGraphError::Corruption(_) => 9,
        RGraphError::Index(_) => 10,
        RGraphError::Storage(_) => 11,
        RGraphError::Transaction(_) => 12,
        RGraphError::ResourceExhausted(_) => 13,
        RGraphError::Internal(_) => 101,
    }
}

fn main() {
    let cli = Cli::parse();

    let verbose = cli.verbose >= 2 || cli.verbose == 1;
    let quiet = cli.quiet;
    let json_log = cli.json_log;

    rgraph::telemetry::init_subscriber(json_log, verbose, quiet);

    let fs = PosixFileSystem::new(false);

    match cli.cmd {
        Command::Init { path } => {
            let _span = tracing::info_span!("cmd", command = "init").entered();
            match Database::init(&path, &fs, rgraph::config::GraphMode::Lpg) {
                Ok(db) => {
                    info!("Database initialised at {:?}", path);
                    println!(
                        "Database initialised at {:?}",
                        path
                    );
                    println!(
                        "  total pages: {}",
                        db.page_manager().superblock.total_page_count
                    );
                }
                Err(e) => {
                    tracing::error!("Database init failed: {}", e);
                    eprintln!("Error: {}", e);
                    let rerr: rgraph::error::RGraphError = e.into();
                    std::process::exit(exit_code_for(&rerr));
                }
            }
        }
        Command::Open { path } => {
            let _span = tracing::info_span!("cmd", command = "open").entered();
            match Database::open(&path, &fs, rgraph::config::GraphMode::Lpg) {
                Ok(db) => {
                    info!("Database opened at {:?}", path);
                    println!("Database opened at {:?}", path);
                    println!(
                        "  total pages: {}",
                        db.page_manager().superblock.total_page_count
                    );
                    println!(
                        "  free pages:  {}",
                        db.page_manager().superblock.free_page_count
                    );
                    println!(
                        "  WAL LSN:     {}",
                        db.page_manager().superblock.current_wal_lsn
                    );
                }
                Err(e) => {
                    tracing::error!("Database open failed: {}", e);
                    eprintln!("Error: {}", e);
                    let rerr: rgraph::error::RGraphError = e.into();
                    std::process::exit(exit_code_for(&rerr));
                }
            }
        }
        Command::Insert { path, data } => {
            let _span = tracing::info_span!("cmd", command = "insert").entered();
            match Database::open(&path, &fs, rgraph::config::GraphMode::Lpg) {
                Ok(mut db) => {
                    let pid = db.page_manager_mut().allocate_page();
                    let mut page = SlottedPage::init(pid, PageType::SlottedData);
                    let idx = page.insert(data.as_bytes()).expect("record fits in page");
                    page.update_checksum();
                    db.page_manager_mut()
                        .write_page(&fs, pid, &mut page.buf)
                        .expect("write page");

                    let payload = {
                        let mut p = pid.to_be_bytes().to_vec();
                        p.extend_from_slice(&page.buf);
                        p
                    };
                    let rec = rgraph::wal::record::WalRecord::new(
                        rgraph::wal::record::RecordType::PageUpdate,
                        1,
                        0,
                        db.page_manager().superblock.current_wal_lsn,
                        payload,
                    );
                    let lsn = db.wal_writer_mut().append(&fs, rec).expect("append wal");
                    db.wal_writer_mut().sync(&fs).expect("sync wal");
                    db.page_manager_mut().superblock.current_wal_lsn = lsn;
                    db.page_manager_mut().sync_superblock(&fs).expect("sync meta");

                    info!("Inserted record into page {} slot {}", pid, idx);
                    println!("Inserted record into page {} slot {}", pid, idx);
                }
                Err(e) => {
                    tracing::error!("Database insert failed: {}", e);
                    eprintln!("Error: {}", e);
                    let rerr: rgraph::error::RGraphError = e.into();
                    std::process::exit(exit_code_for(&rerr));
                }
            }
        }
        Command::Read {
            path,
            page_id,
            slot,
        } => {
            let _span = tracing::info_span!("cmd", command = "read").entered();
            match Database::open(&path, &fs, rgraph::config::GraphMode::Lpg) {
                Ok(db) => {
                    let mut buf = rgraph::io::AlignedBuffer::zeroed(
                        rgraph::storage::page::PAGE_SIZE
                    );
                    if let Err(e) = db.page_manager().read_page(&fs, page_id, &mut buf) {
                        tracing::error!("Error reading page: {}", e);
                        eprintln!("Error reading page: {}", e);
                        let rerr: rgraph::error::RGraphError = e.into();
                        std::process::exit(exit_code_for(&rerr));
                    }
                    let page = SlottedPage::new(buf);
                    match page.read(slot) {
                        Some(data) => {
                            println!(
                                "Page {} slot {}: {:?}",
                                page_id,
                                slot,
                                String::from_utf8_lossy(data)
                            );
                        }
                        None => println!("No record at page {} slot {}", page_id, slot),
                    }
                }
                Err(e) => {
                    tracing::error!("Database read failed: {}", e);
                    eprintln!("Error: {}", e);
                    let rerr: rgraph::error::RGraphError = e.into();
                    std::process::exit(exit_code_for(&rerr));
                }
            }
        }
        Command::Serve {
            host,
            port,
            workers,
            max_connections,
            cpu_threads,
            path,
            tls,
            tls_cert,
            tls_key,
        } => {
            let _span = tracing::info_span!("cmd", command = "serve").entered();
            let worker_threads = if workers == 0 {
                num_cpus::get().max(2)
            } else {
                workers
            };
            let cpu_threads = if cpu_threads == 0 {
                num_cpus::get().max(2)
            } else {
                cpu_threads
            };

            let config = ServerConfig {
                worker_threads,
                shutdown_timeout_secs: 30,
                host: host.clone(),
                port,
                tls_enabled: tls,
                tls_cert_path: tls_cert,
                tls_key_path: tls_key,
                max_connections,
                cpu_pool_threads: cpu_threads,
            };

            let runtime = ServerRuntime::new(config.clone()).expect("create runtime");
            let in_flight = Arc::new(AtomicUsize::new(0));

            let data_path = path.join("rgraph.db");
            let engine: Arc<dyn AsyncGraphEngine> =
                if data_path.exists() {
                    Arc::new(GraphEngineAdapter::init(data_path).expect("open graph engine"))
                } else {
                    info!("database file not found; initialising new graph engine");
                    Arc::new(GraphEngineAdapter::init(data_path).expect("init graph engine"))
                };

            let metrics = MetricsCollector::new();
            let _dispatcher = RequestDispatcher::new(cpu_threads).expect("create dispatcher");

            info!(
                "starting RGraph server on {}:{} (workers={}, cpu_threads={}, max_conns={})",
                host, port, worker_threads, cpu_threads, max_connections
            );

            let host_clone = host.clone();
            let port_clone = port;
            let max_connections_clone = max_connections;
            let tls_cert_clone = config.tls_cert_path.clone();
            let tls_key_clone = config.tls_key_path.clone();

            runtime.block_on(
                async move {
                    let acceptor = ConnectionAcceptor::bind(
                        &format!("{}:{}", host_clone, port_clone),
                        tls_cert_clone,
                        tls_key_clone,
                        max_connections_clone,
                    )
                    .await?;

                    info!("listening on port {}", acceptor.local_port);

                    let grpc_routes = GraphGrpcServer::routes(engine, metrics);
                    let addr = format!("{}:{}", host_clone, port_clone).parse().unwrap();
                    grpc_routes.serve(addr).await.map_err(|e| {
                        rgraph::error::RGraphError::Io(format!("grpc serve failed: {e}"))
                    })
                },
                in_flight,
            );
        }
        Command::Query { path, query, json } => {
            let _span = tracing::info_span!("cmd", command = "query", path = %path.display()).entered();
            info!("Executing query '{}' on {:?}", query, path);

            // Route through full pipeline when a database path is provided and it
            // exists; otherwise fall back to the expression-only naive executor.
            let result = if path.exists() {
                execute_full_pipeline(&path, &query, &fs)
                    .or_else(|_| execute_expression_query(&query))
            } else {
                execute_expression_query(&query)
            };

            match result {
                Ok(result) => {
                    if json {
                        println!("{}", result.render_json());
                    } else {
                        print!("{}", result.render_table());
                    }
                }
                Err(e) => {
                    tracing::error!("Query execution failed: {}", e);
                    eprintln!("Error: {}", e);
                    let rerr: rgraph::error::RGraphError = e.into();
                    std::process::exit(exit_code_for(&rerr));
                }
            }
        }
        Command::Import { path, file, format } => {
            let _span = tracing::info_span!("cmd", command = "import", file = %file.display(), format = %format).entered();
            info!("Importing {:?} as {} into {:?}", file, format, path);
            println!("Import from {:?} (format: {}) into {:?}", file, format, path);
            println!("Import is a stub — full implementation depends on the query execution engine (Sprint 21).");
        }
        Command::Export { path, output, format, label } => {
            let _span = tracing::info_span!("cmd", command = "export", output = %output.display(), format = %format).entered();
            info!("Exporting {:?} as {} to {:?}", path, format, output);
            println!("Export to {:?} (format: {}) from {:?}", output, format, path);
            if let Some(l) = label {
                println!("  label filter: {}", l);
            }
            println!("Export is a stub — full implementation depends on the query execution engine (Sprint 21).");
        }
        Command::Benchmark { path, iterations, concurrency } => {
            let _span = tracing::info_span!("cmd", command = "benchmark", iterations = iterations, concurrency = concurrency).entered();
            info!("Running benchmark on {:?} (iterations={}, concurrency={})", path, iterations, concurrency);
            println!("Benchmark on {:?}", path);
            println!("  iterations: {}", iterations);
            println!("  concurrency: {}", concurrency);
            println!("Benchmark is a stub — full implementation depends on the query execution engine (Sprint 21).");
        }
    }
}

/// Execute a Cypher query through the full parse → semantic → plan → physical pipeline.
///
/// Opens the database at `path`, routes the query through:
/// `parse → semantic_analyse → planner::plan → physical::execute_plan`
/// and returns the result.
fn execute_full_pipeline(
    path: &std::path::Path,
    query: &str,
    fs: &impl rgraph::io::FileSystem,
) -> Result<rgraph::cypher::executor::QueryResult, rgraph::cypher::executor::ExecError> {
    use rgraph::cypher::executor::ExecError;

    let mut engine = GraphStorageEngine::open(path.to_path_buf(), fs)
        .map_err(|e| ExecError::Eval(format!("failed to open database: {}", e)))?;

    let stmt = parse(query)
        .map_err(|e| ExecError::Eval(format!("parse error: {}", e)))?;

    let _ = analyse(&stmt)
        .map_err(|e| ExecError::Semantic(e.to_string()))?;

    let logical_plan = plan(&stmt)
        .map_err(|e| ExecError::Eval(format!("plan error: {}", e)))?;

    let ctx = ExecutionContext::new_with_write(&mut engine, fs);
    execute_plan(&logical_plan, &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rgraph::error::RGraphError;

    #[test]
    fn exit_codes_are_distinct() {
        let variants = vec![
            RGraphError::Syntax("x".into()),
            RGraphError::Semantic("x".into()),
            RGraphError::Type("x".into()),
            RGraphError::Argument("x".into()),
            RGraphError::NotFound("x".into()),
            RGraphError::AlreadyExists("x".into()),
            RGraphError::Io("x".into()),
            RGraphError::Corruption("x".into()),
            RGraphError::Index("x".into()),
            RGraphError::Storage("x".into()),
            RGraphError::Transaction("x".into()),
            RGraphError::ResourceExhausted("x".into()),
            RGraphError::Internal("x".into()),
        ];
        let mut codes = std::collections::HashSet::new();
        for err in &variants {
            let code = exit_code_for(err);
            assert!(
                codes.insert(code),
                "duplicate exit code {} for {:?}",
                code,
                err
            );
        }
        assert_eq!(codes.len(), variants.len());
    }

    #[test]
    fn internal_error_uses_bug_exit_code() {
        let err = RGraphError::Internal("oops".into());
        assert_eq!(exit_code_for(&err), 101);
    }
}
