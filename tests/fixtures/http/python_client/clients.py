import httpx
import requests

BASE = "https://api.example.com"


def sync(user_id):
    requests.get(f"{BASE}/users/{user_id}")
    requests.post("https://api.example.com/users", json={})
    requests.request("PUT", "/users/7")
    httpx.delete(f"/users/{user_id}/sessions", timeout=3)
    with httpx.Client() as client:
        client.get("https://api.example.com/orders?page=2")
        # Not requests: relative URL through an instance, a dict read, a URL in a variable.
        client.get("/relative")
    config = {}
    config.get("timeout")
    requests.get(BASE)
