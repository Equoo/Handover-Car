use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{debug, error};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub struct AsyncQueue<T> {
    tx: mpsc::UnboundedSender<Message<T>>,
}

enum Message<T> {
    Task(BoxFuture<T>),
    Shutdown,
}

impl<T: Send + 'static> AsyncQueue<T> {
    pub fn new() -> (Self, AsyncQueueHandle<T>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<Message<T>>();
        let (result_tx, result_rx) = mpsc::unbounded_channel::<T>();

        // Spawn executor task
        tokio::spawn(async move {
            let mut join_set = JoinSet::new();

            loop {
                tokio::select! {
                    // Receive new messages
                    Some(msg) = rx.recv() => {
                        match msg {
                            Message::Task(task) => {
                                join_set.spawn(task);
                            }
                            Message::Shutdown => {
                                debug!("Shutting down executor...");
                                break;
                            }
                        }
                    }

                    // Process completed tasks
                    Some(result) = join_set.join_next() => {
                        match result {
                            Ok(value) => {
                                result_tx.send(value).ok();
                            }
                            Err(e) => error!("Task error: {:?}", e),
                        }
                    }
                }
            }

            // Wait for remaining tasks
            while let Some(result) = join_set.join_next().await {
                if let Ok(value) = result {
                    result_tx.send(value).ok();
                }
            }

            debug!("Executor shut down cleanly");
        });

        (Self { tx: tx.clone() }, AsyncQueueHandle { tx, result_rx })
    }

    pub fn submit<F>(&self, task: F)
    where
        F: Future<Output = T> + Send + 'static,
    {
        self.tx.send(Message::Task(Box::pin(task))).ok();
    }

    pub fn shutdown(&self) {
        self.tx.send(Message::Shutdown).ok();
    }
}

impl<T> Clone for AsyncQueue<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

pub struct AsyncQueueHandle<T> {
    tx: mpsc::UnboundedSender<Message<T>>,
    result_rx: mpsc::UnboundedReceiver<T>,
}

impl<T> AsyncQueueHandle<T> {
    pub async fn recv(&mut self) -> Option<T> {
        self.result_rx.recv().await
    }

    pub fn shutdown(&self) {
        self.tx.send(Message::Shutdown).ok();
    }
}


// Usage
// #[tokio::main]
// async fn main() {
//     let (queue, mut handle) = AsyncQueue::<String>::new();
//
//     // Can clone and share queue
//     let queue_clone = queue.clone();
//
//     // Submit tasks from multiple places
//     queue.submit(async {
//         tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
//         "Task 1".to_string()
//     });
//
//     queue_clone.submit(async {
//         tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
//         "Task 2".to_string()
//     });
//
//     // Collect results
//     tokio::spawn(async move {
//         while let Some(result) = handle.recv().await {
//             println!("Got result: {}", result);
//         }
//     });
//
//     tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
//
//     // Graceful shutdown
//     queue.shutdown();
//
//     tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
// }
