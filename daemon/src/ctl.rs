use std::collections::HashMap;
use zbus::Connection;
use zvariant::OwnedValue;

fn format_val(v: &OwnedValue) -> String {
    if let Ok(s) = v.downcast_ref::<String>() {
        if s.is_empty() {
            "(none)".into()
        } else {
            s.clone()
        }
    } else if let Ok(n) = v.downcast_ref::<u64>() {
        n.to_string()
    } else if let Ok(n) = v.downcast_ref::<f64>() {
        format!("{:.2}", n)
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
    let conn = match Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot connect to session bus: {e}");
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
            eprintln!("error: cannot query daemon: {e}");
            eprintln!("Is umbriel-vram-booster running?");
            std::process::exit(1);
        }
    };

    let total = get_u64(&props, "VramTotal").unwrap_or(0);
    let boosted = get_u64(&props, "BoostedBytes").unwrap_or(0);
    let boost_ratio = get_f64(&props, "BoostRatio").unwrap_or(0.0);

    println!("=== Umbriel VRAM Booster Status ===");
    println!("Daemon:           running");
    println!(
        "DRM key:          {}",
        props.get("DrmKey").map_or("?".into(), format_val)
    );
    println!("VRAM total:       {}", human_bytes(total));
    println!("Boost ratio:      {:.0}%", boost_ratio * 100.0);
    println!(
        "Boosted bytes:    {} ({}% of total)",
        human_bytes(boosted),
        if total > 0 {
            (boosted as f64 / total as f64 * 100.0) as u64
        } else {
            0
        }
    );
    println!(
        "Current unit:     {}",
        props.get("CurrentUnit").map_or("(none)".into(), format_val)
    );
    println!(
        "Previous cgroup:  {}",
        props.get("PrevCgroup").map_or("(none)".into(), format_val)
    );
}
