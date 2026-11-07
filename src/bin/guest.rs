fn main() {
    if let Err(error) = firecracker_server::guest_agent::run() {
        let code = firecracker_server::guest_agent::category(&error);
        println!("{}", serde_json::json!({"event":"failed","category":code}));
        std::process::exit(1);
    }
}
