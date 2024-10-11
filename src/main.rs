//           ,____           \'/
//       .-'` .   |        -= * =-
//     .'  '    ./           /.\
//    /  '    .'        
//   ;  '    /           
//  :  '  _ ;            
// ;  :  /(\ \
// |  .       '.
// |  ' /     --'
// |  .   '.__\
// ;  :       /
//  ;  .     |            ,
//   ;  .    \           /|
//    \  .    '.       .'/
//     '.  '  . `'---'`.'
//       `'-..._____.-`
//
// Care about the emission. It’s freedom in code.
// Just a pulse in the network, a chance to be heard.
//
mod vdf_solution;
mod ascii_art;
mod models;
mod hasher;
mod utils;
mod api;

use utils::*;
use models::*;
use hasher::*;
use colored::*;
use std::thread;
use ascii_art::*;
use std::sync::Arc;
use std::process::exit;
use std::time::Duration;
use tokio::sync::{Mutex};
use crate::api::MinerState;
use vdf_solution::HCGraphUtil;
use futures_util::{StreamExt, SinkExt};
use std::sync::{atomic::{AtomicUsize, Ordering}, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[tokio::main]
async fn main() {
    let args = Args::parse_and_validate();
    std::panic::set_hook(Box::new(|_info| {}));

    let max_workers = num_cpus::get();
    assert!(max_workers > 0);

    let num_workers = match args.threads {
        Some(t) => {
            if t >= max_workers {
                println!("{}", "Requested number of threads exceeds available cores. Using maximum allowed".bold().red());
                max_workers
            } else {
                t
            }
        }
        None => max_workers,
    };

    println!("{}", "STARTING MINER".bold().green());
    println!("{} {}", "USING WORKERS: ".bold().cyan(), format!("{}", num_workers).bold().cyan());
    print_startup_art();

    // Handle Ctrl+C signal
    tokio::spawn(handle_exit_signals());

    let bailout_timer = args.vdftime_parsed;
    let miner_id = args.address.unwrap();
    let request = args.pool.unwrap().clone().into_client_request().unwrap();
    let (ws_stream, _) = connect_async(request)
        .await
        .expect("Failed to connect to WebSocket server");

    let (write, mut read) = ws_stream.split();
    let (server_sender, server_receiver) = mpsc::channel::<String>();

    let current_job: Arc<Mutex<Option<Job>>> = Arc::new(Mutex::new(None));

    let miner_state = Arc::new(MinerState {
        hash_count: Arc::new(AtomicUsize::new(0)),
        accepted_shares: Arc::new(AtomicUsize::new(0)),
        rejected_shares: Arc::new(AtomicUsize::new(0)),
        hashrate_samples: Arc::new(Mutex::new(Vec::new())),
        version: String::from("1.0.0"),
    });

    // Spawn write task to send solutions to the server
    tokio::spawn(async move {
        let mut write = write;
        while let Ok(msg) = server_receiver.recv() {
            write.send(Message::Text(msg)).await.unwrap();
        }
    });

    // Spawn worker threads for processing jobs
    let hash_count = Arc::new(AtomicUsize::new(0));
    for _ in 0..num_workers {
        let current_job_loop = Arc::clone(&current_job);
        let hash_count = Arc::clone(&hash_count);
        let server_sender_clone = server_sender.clone();
        let miner_id = miner_id.clone();
        let api_hash_count = Arc::clone(&miner_state.hash_count);

        thread::spawn(move || {
            let mut hc_util = HCGraphUtil::new(bailout_timer);
            loop {
                let job_option = {
                    let job_guard = current_job_loop.blocking_lock();
                    job_guard.clone()
                };

                if let Some(job) = job_option {
                    loop {
                        let nonce = generate_nonce();

                        if let Some((hash, path_hex)) = compute_hash_no_vdf(&("".to_owned() + &job.data + &nonce), &mut hc_util) {
                            hash_count.fetch_add(1, Ordering::Relaxed);
                            api_hash_count.fetch_add(1, Ordering::Relaxed);

                            if meets_target(&hash, &job.target) {
                                let submit_msg = SubmitMessage {
                                    r#type: String::from("submit"),
                                    miner_id: miner_id.to_string(),
                                    nonce: nonce,
                                    job_id: job.job_id.clone(),
                                    path: path_hex,
                                };

                                let msg = serde_json::to_string(&submit_msg).unwrap();
                                let _ = server_sender_clone.send(msg);

                                // Clear the current job
                                let mut job_guard = current_job_loop.blocking_lock();
                                *job_guard = None;
                                break;
                            }

                            // Check if there's a new job
                            let new_job_option = {
                                let job_guard = current_job_loop.blocking_lock();
                                job_guard.clone()
                            };

                            if new_job_option.is_none() || new_job_option.unwrap().job_id != job.job_id {
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    // Spawn hash rate monitoring task
    tokio::spawn(async move {
        let mut last_count = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let count = hash_count.load(Ordering::Relaxed);
            println!("{}: {} hashes/second", "Hash rate".cyan(), (count - last_count) / 5);
            last_count = count;
        }
    });

    // Spawn api hash rate monitoring task
    let api_hashrate_clone = miner_state.clone();
    tokio::spawn(async move {
        let mut last_count = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await; // Measure every second
            let current_count = api_hashrate_clone.hash_count.load(Ordering::Relaxed);
            let hashes_per_second = current_count - last_count;
            let mut samples = api_hashrate_clone.hashrate_samples.lock().await;
            samples.push(hashes_per_second as u64);
            if samples.len() > 10 {
                samples.remove(0);
            }
            last_count = current_count;
        }
    });


    let api_state = miner_state.clone();
    tokio::spawn(api::start_http_server(api_state));

    // Run the read task
    let current_job_clone = Arc::clone(&current_job);
    
    loop {
        match read.next().await {
            Some(Ok(msg)) => {
                let server_message: ServerMessage =
                    serde_json::from_str(msg.to_text().unwrap()).unwrap();
                match server_message.r#type.as_str() {
                    "job" => {
                        if let (Some(job_id), Some(data), Some(target)) = (
                            server_message.job_id.clone(),
                            server_message.data.clone(),
                            server_message.target.clone(),
                        ) {
                            let new_job = Job {
                                job_id: job_id.clone(),
                                data: data.clone(),
                                target: target.clone(),
                            };

                            let mut job_guard = current_job_clone.lock().await;
                            *job_guard = Some(new_job);

                            println!(
                                "{} {}",
                                "Received new job:".bold().blue(),
                                format!(
                                    "ID = {}, Data = {}, Target = {}",
                                    job_id, data, target
                                )
                                .bold()
                                .yellow()
                            );
                        }
                    }
                    "accepted" => {
                        miner_state.accepted_shares.fetch_add(1, Ordering::Relaxed);
                        println!(
                            "{}",
                            format!("Share accepted")
                                .bold()
                                .green()
                        );
                        display_share_accepted();
                    }
                    "rejected" => {
                        miner_state.rejected_shares.fetch_add(1, Ordering::Relaxed);
                        println!("{}", "Share rejected.".red());
                    }
                    _ => {}
                }
            }
            Some(Err(e)) => {
                println!("Error receiving message: {:?}", e);
                exit(0);
            }
            None => {
                println!("WebSocket connection closed.");
                exit(0);
            }
        }
    }
}
