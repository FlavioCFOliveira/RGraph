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

#[derive(Parser)]
#[command(name = "rgraph")]
#[command(about = "RGraph - A high-performance graph database engine")]
struct Cli {
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
}

fn main() {
    let cli = Cli::parse();
    let fs = PosixFileSystem::new(false);

    match cli.cmd {
        Command::Init { path } => {
            match Database::init(&path, &fs) {
                Ok(db) => {
                    println!("Database initialised at {:?}", path);
                    println!(
                        "  total pages: {}",
                        db.page_manager.superblock.total_page_count
                    );
                }
                Err(e) => eprintln!("Error: {}", e),
            }
        }
        Command::Open { path } => {
            match Database::open(&path, &fs) {
                Ok(db) => {
                    println!("Database opened at {:?}", path);
                    println!(
                        "  total pages: {}",
                        db.page_manager.superblock.total_page_count
                    );
                    println!(
                        "  free pages:  {}",
                        db.page_manager.superblock.free_page_count
                    );
                    println!(
                        "  WAL LSN:     {}",
                        db.page_manager.superblock.current_wal_lsn
                    );
                }
                Err(e) => eprintln!("Error: {}", e),
            }
        }
        Command::Insert { path, data } => {
            match Database::open(&path, &fs) {
                Ok(mut db) => {
                    let pid = db.page_manager.allocate_page();
                    let mut page = SlottedPage::init(pid, PageType::SlottedData);
                    let idx = page.insert(data.as_bytes()).expect("record fits in page");
                    page.update_checksum();
                    db.page_manager
                        .write_page(&fs, pid, &page.buf)
                        .expect("write page");

                    // Simple WAL record for the page update.
                    let payload = {
                        let mut p = pid.to_be_bytes().to_vec();
                        p.extend_from_slice(&page.buf);
                        p
                    };
                    let rec = rgraph::wal::record::WalRecord::new(
                        rgraph::wal::record::RecordType::PageUpdate,
                        1,
                        0,
                        db.page_manager.superblock.current_wal_lsn,
                        payload,
                    );
                    let lsn = db.wal_writer.append(&fs, rec).expect("append wal");
                    db.wal_writer.sync(&fs).expect("sync wal");
                    db.page_manager.superblock.current_wal_lsn = lsn;
                    db.page_manager.sync_superblock(&fs).expect("sync meta");

                    println!("Inserted record into page {} slot {}", pid, idx);
                }
                Err(e) => eprintln!("Error: {}", e),
            }
        }
        Command::Read {
            path,
            page_id,
            slot,
        } => {
            match Database::open(&path, &fs) {
                Ok(db) => {
                    let mut buf = rgraph::io::AlignedBuffer::zeroed(
                        rgraph::storage::page::PAGE_SIZE
                    );
                    if let Err(e) = db.page_manager.read_page(&fs, page_id, &mut buf) {
                        eprintln!("Error reading page: {}", e);
                        return;
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
                Err(e) => eprintln!("Error: {}", e),
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

            // Initialise graph engine adapter.
            let data_path = path.join("rgraph.db");
            let engine: Arc<dyn AsyncGraphEngine> =
                if data_path.exists() {
                    Arc::new(GraphEngineAdapter::init(data_path).expect("open graph engine"))
                } else {
                    // Fallback to in-memory adapter for quick testing.
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

                    // For Sprint 23, we serve gRPC directly via tonic's transport
                    // server instead of the manual acceptor loop.  The acceptor
                    // module is fully tested and ready for the future Bolt layer.
                    let grpc_routes = GraphGrpcServer::routes(engine, metrics);
                    let addr = format!("{}:{}", host_clone, port_clone).parse().unwrap();
                    grpc_routes.serve(addr).await.map_err(|e| {
                        rgraph::error::RGraphError::Io(format!("grpc serve failed: {e}"))
                    })
                },
                in_flight,
            );
        }
    }
}
