//! Per-adapter collection summary. Handy for sanity-checking the
//! pipeline against a live system without bringing up the TUI.

use sta_adapters::{AnacronAdapter, AtAdapter, CronAdapter, LaunchdAdapter, SystemdAdapter};
use sta_core::{Error, TaskSource};

fn main() {
    let sources: Vec<Box<dyn TaskSource>> = vec![
        Box::new(SystemdAdapter::new()),
        Box::new(CronAdapter::new()),
        Box::new(AtAdapter::new()),
        Box::new(AnacronAdapter::new()),
        Box::new(LaunchdAdapter::new()),
    ];

    for source in &sources {
        let label = source.kind().as_str();
        match source.collect() {
            Ok(tasks) => {
                println!("[{label}] {} tasks", tasks.len());
                for t in tasks.iter().take(3) {
                    println!("    - {} :: {:?} :: {}", t.name, t.schedule, t.command);
                }
                if tasks.len() > 3 {
                    println!("    ... and {} more", tasks.len() - 3);
                }
            }
            Err(Error::Unavailable(msg)) => println!("[{label}] unavailable ({msg})"),
            Err(e) => println!("[{label}] error: {e}"),
        }
    }
}
