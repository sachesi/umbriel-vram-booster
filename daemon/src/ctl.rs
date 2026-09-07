use std::collections::HashMap;
use zbus::Connection;
use zvariant::OwnedValue;

const USAGE: &str = "\
umbriel-vram-boosterctl - show what umbriel-vram-booster is doing

Usage: umbriel-vram-boosterctl [--help] [--version]

Prints the daemon's GPU, boost size, the socket it follows and the unit that
currently holds the boost. Takes no other arguments.";

/// Unit names and cgroup paths come from other processes, so they can carry
/// control characters that would rewrite the terminal. Strip them.
fn printable(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_control()).collect()
}

fn format_val(v: &OwnedValue) -> String {
    if let Ok(s) = v.downcast_ref::<String>() {
        if s.is_empty() {
            "(none)".into()
        } else {
            printable(&s)
        }
    } else if let Ok(n) = v.downcast_ref::<u64>() {
        n.to_string()
    } else if let Ok(n) = v.downcast_ref::<f64>() {
        format!("{n:.2}")
    } else {
        format!("{v:?}")
    }
}

fn human_bytes(b: u64) -> String {
    let mb = b / (1024 * 1024);
    let gb = mb as f64 / 1024.0;
    if gb >= 1.0 {
        format!("{b} ({mb} MiB, {gb:.2} GiB)")
    } else {
        format!("{b} ({mb} MiB)")
    }
}

fn get_u64(props: &HashMap<String, OwnedValue>, key: &str) -> Option<u64> {
    props.get(key).and_then(|v| v.downcast_ref::<u64>().ok())
}

fn get_f64(props: &HashMap<String, OwnedValue>, key: &str) -> Option<f64> {
    props.get(key).and_then(|v| v.downcast_ref::<f64>().ok())
}

#[tokio::main]
async fn main() {
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return;
        }
        Some("--version" | "-V") => {
            println!("umbriel-vram-boosterctl {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some(other) => {
            eprintln!("error: unknown argument {other:?}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }

    let conn = match Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot connect to the session bus: {e}");
            std::process::exit(1);
        }
    };

    let result: Result<HashMap<String, OwnedValue>, _> = conn
        .call_method(
            Some("org.umbriel.VramBooster"),
            "/org/umbriel/VramBooster",
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &("org.umbriel.VramBooster",),
        )
        .await
        .and_then(|r| r.body().deserialize());

    let props = match result {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot query the daemon: {e}");
            eprintln!("check it with: systemctl --user status umbriel-vram-booster.service");
            std::process::exit(1);
        }
    };

    let total = get_u64(&props, "VramTotal").unwrap_or(0);
    let boosted = get_u64(&props, "BoostedBytes").unwrap_or(0);
    let boost_ratio = get_f64(&props, "BoostRatio").unwrap_or(0.0);
    let following = props.get("Following").map(format_val);

    println!("=== Umbriel VRAM Booster Status ===");
    println!("Daemon:           running");
    println!(
        "Following:        {}",
        match following.as_deref() {
            Some("(none)") | None => "(not connected - waiting for Umbriel)",
            Some(path) => path,
        }
    );
    println!(
        "DRM key:          {}",
        props.get("DrmKey").map_or("?".into(), format_val)
    );
    println!("VRAM total:       {}", human_bytes(total));
    println!("Boost ratio:      {:.0}%", boost_ratio * 100.0);
    println!("Boosted bytes:    {}", human_bytes(boosted));
    println!(
        "Current unit:     {}",
        props.get("CurrentUnit").map_or("(none)".into(), format_val)
    );
    println!(
        "Boosted cgroup:   {}",
        props
            .get("BoostedCgroup")
            .map_or("(none)".into(), format_val)
    );
}
