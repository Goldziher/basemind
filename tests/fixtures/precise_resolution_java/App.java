package com.example;

import com.example.other.Foo;

public class App {
    private int value = 1;

    public int fieldVsLocal() {
        int value = 2;
        return value;
    }

    public int readField() {
        return this.value;
    }

    public int firstParam(int value) {
        return value;
    }

    public int secondParam(int value) {
        return value * 2;
    }

    public String greet() {
        return "App.greet";
    }

    public String callsImportedClass() {
        return Foo.greet();
    }
}
