package main

import "fmt"

// GoStruct is a sample struct
type GoStruct struct {
	Name string
}

// GetName returns the name
func (s *GoStruct) GetName() string {
	return s.Name
}

func main() {
	s := GoStruct{Name: "test"}
	fmt.Println(s.GetName())
}
