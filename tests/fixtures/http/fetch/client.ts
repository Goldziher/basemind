import axios from "axios";

const BASE = "https://api.example.com";

export async function load(id: string): Promise<void> {
  await fetch(`${BASE}/users/${id}`);
  await fetch("/api/users", { method: "POST", body: "{}" });
  await axios.get(`/api/users/${id}`);
  await axios.delete("https://api.example.com/api/cache/k");
  await axios({ method: "put", url: "/api/users/7" });
  const api = axios.create();
  await api.get("https://svc.internal/orders?limit=10");
  // Not requests: no literal URL, a relative path through an unknown receiver, a map read.
  await fetch(someUrl);
  await api.get("/relative");
  new Map().get("key");
}
