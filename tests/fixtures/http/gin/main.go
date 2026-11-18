package main

import "github.com/gin-gonic/gin"

type handlers struct{}

func (handlers) List(c *gin.Context) {}

func showUser(c *gin.Context) {}

func setupRouter() *gin.Engine {
	r := gin.Default()
	h := handlers{}
	r.GET("/ping", func(c *gin.Context) { c.String(200, "pong") })
	api := r.Group("/api")
	v1 := api.Group("/v1")
	v1.GET("/users", h.List)
	v1.GET("/users/:id", auth, showUser)
	v1.POST("/users", h.List)
	r.Any("/static/*filepath", h.List)
	return r
}
