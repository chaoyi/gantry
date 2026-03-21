pub async fn status(
    host: &str,
    kind: Option<&str>,
    name: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = match (kind, name) {
        (Some(k), Some(n)) => format!("{host}/api/status/{k}/{n}"),
        _ => format!("{host}/api/status"),
    };
    let resp = reqwest::get(&url).await?;
    let body: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

pub async fn post_op(
    host: &str,
    path: &str,
    timeout: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = if let Some(t) = timeout {
        format!("{host}{path}?timeout={t}")
    } else {
        format!("{host}{path}")
    };
    let client = reqwest::Client::new();
    let resp = client.post(&url).send().await?;
    let body: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

pub async fn graph(host: &str, target: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let url = match target {
        Some(t) => format!("{host}/api/graph/target/{t}"),
        None => format!("{host}/api/graph"),
    };
    let resp = reqwest::get(&url).await?;
    let body: serde_json::Value = resp.json().await?;

    // Format as structured text
    if let Some(services) = body["services"].as_array() {
        println!("Services:");
        for svc in services {
            let name = svc["name"].as_str().unwrap_or("?");
            let state = svc["state"].as_str().unwrap_or("?");
            println!("  {name} [{state}]");
            if let Some(probes) = svc["probes"].as_array() {
                for probe in probes {
                    let probe_name = probe["name"].as_str().unwrap_or("?");
                    let probe_state = probe["state"].as_str().unwrap_or("?");
                    let deps = probe["depends_on"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    let dep_str = if deps.is_empty() {
                        String::new()
                    } else {
                        format!("  depends_on: {deps}")
                    };
                    let dot = match probe_state {
                        "green" => "●",
                        "red" => "●",
                        "stale" => "●",
                        _ => "○",
                    };
                    println!("    {dot} {probe_name:<12} {probe_state}{dep_str}");
                }
            }
            if let Some(start_after) = svc["start_after"].as_array()
                && !start_after.is_empty()
            {
                let deps: Vec<&str> = start_after.iter().filter_map(|v| v.as_str()).collect();
                println!("    start_after: {}", deps.join(", "));
            }
            println!();
        }
    }

    if let Some(targets) = body["targets"].as_array() {
        println!("Targets:");
        for tgt in targets {
            let name = tgt["name"].as_str().unwrap_or("?");
            let state = tgt["state"].as_str().unwrap_or("?");
            let probe_list = tgt["probes"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            println!("  {name} [{state}]  probes: {probe_list}");
        }
    }

    Ok(())
}
