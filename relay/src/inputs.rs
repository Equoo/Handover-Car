async fn event_loop() {
    println!("p{} ended the session", player_id);
}

fn main() {
    pasts::block_on(event_loop());
}
