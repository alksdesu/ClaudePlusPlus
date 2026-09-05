fn main() {
    let status = claude_plus_plus_lib::fastmode::status();
    println!("{}", serde_json::to_string_pretty(&status).unwrap());
}
