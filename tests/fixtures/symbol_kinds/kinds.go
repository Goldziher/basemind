package kinds

import "fmt"

type ID = string

type (
	Name   = string
	Record struct{}
)

type Store interface {
	Get(id ID) (Record, error)
	Put(r Record) error
	fmt.Stringer
}

func Use(s Store) {
	var probe interface{ Close() error }
	_ = probe
}
