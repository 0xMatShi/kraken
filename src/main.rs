mod models; mod scanner; mod websocket; mod engine; mod report;
use std::io::{self, Write};
use std::sync::Arc;
use scanner::AutoScanner;
use websocket::DataStream;
use engine::DryRunEngine;

#[tokio::main]
async fn main() {
    let scanner = AutoScanner::new();
    println!("\n1. Start Strategy | 2. Exit");
    print!("> "); io::stdout().flush().unwrap();
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    loop {
        if input.trim() == "1" {
            if let Some(target) = scanner.find_next_target("btc-updown-15m", 0.0, 15.0).await {
                let engine = Arc::new(DryRunEngine::new(&target.slug, 700));
                let stream = DataStream::new(target.up_token, target.down_token, engine);
                if let Err(e) = stream.start_stream(target.end_date).await {
                    println!("Error: {}", e);
                }
            }
        } else { break; }
    }
}