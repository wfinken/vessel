use std::fs;

fn is_zombie(pid: u32) -> bool {
    let stat_path = format!("/proc/{}/stat", pid);
    if let Ok(stat) = fs::read_to_string(&stat_path) {
        if let Some(rparen) = stat.rfind(')') {
            let after_paren = &stat[rparen + 1..];
            let parts: Vec<&str> = after_paren.split_whitespace().collect();
            if parts.get(0) == Some(&"Z") || parts.get(0) == Some(&"X") {
                return true;
            }
        }
    }
    false
}

fn main() {
    println!("{}", is_zombie(std::process::id()));
}
