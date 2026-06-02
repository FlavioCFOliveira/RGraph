use clap::{Parser, Subcommand};
use rgraph::db::database::Database;
use rgraph::io::posix::PosixFileSystem;
use rgraph::storage::page::{SlottedPage, PageType};
use std::path::PathBuf;

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
    Read { path: PathBuf, page_id: u64, slot: u16 },
}

fn main() {
    let cli = Cli::parse();
    let fs = PosixFileSystem::new(false);

    match cli.cmd {
        Command::Init { path } => {
            match Database::init(&path, &fs) {
                Ok(db) => {
                    println!(
                        "Database initialised at {:?}",
                        path
                    );
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
                    let idx = page.insert(data.as_bytes())
                        .expect("record fits in page");
                    page.update_checksum();
                    db.page_manager.write_page(&fs, pid, &page.buf
                    ).expect("write page");

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
        Command::Read { path, page_id, slot } => {
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
    }
}
