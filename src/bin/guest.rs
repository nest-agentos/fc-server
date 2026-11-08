fn main() {
    let started = std::time::Instant::now();
    if let Err(error) = firecracker_server::guest_agent::run() {
        let code = firecracker_server::guest_agent::category(&error);
        let message = error.to_string();
        let detail = if message.starts_with("ENVIRONMENT_")
            && message.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
        {
            message.as_str()
        } else {
            code
        };
        println!(
            "{}",
            serde_json::json!({"event":"failed","category":code,"code":detail,
            "operationId":uuid::Uuid::new_v4().to_string(),"stage":"guest.prepare_or_run",
            "result":"failure","elapsedMs":started.elapsed().as_millis()})
        );
        std::process::exit(1);
    }
}
