package main

import (
	"context"
	"net/http"
)

func call(ctx context.Context, client *http.Client) {
	http.Get("https://api.example.com/api/v1/users")
	http.Post("/api/v1/users", "application/json", nil)
	http.NewRequest("DELETE", "https://api.example.com/api/v1/users/9", nil)
	http.NewRequestWithContext(ctx, http.MethodPut, "/api/v1/users/9", nil)
	client.Get("https://api.example.com/ping")
	// Not requests: a URL in a variable and a relative path through a client instance.
	url := "https://example.com"
	http.Get(url)
	client.Get("/relative")
}
