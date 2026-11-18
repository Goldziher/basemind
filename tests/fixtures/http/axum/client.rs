pub async fn call(client: &reqwest::Client) -> reqwest::Result<()> {
    reqwest::get("https://api.example.com/users").await?;
    client.post("https://api.example.com/users/42/sessions").send().await?;
    client
        .request(reqwest::Method::PATCH, "http://localhost:3000/users/7")
        .send()
        .await?;
    // Not requests: a map lookup and a relative path through a client instance.
    let map = std::collections::HashMap::<&str, u8>::new();
    let _ = map.get("/users");
    client.get("/relative").send().await?;
    Ok(())
}
