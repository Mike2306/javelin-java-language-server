package com.example;

/** A minimal well-formed compilation unit. */
public class Simple {
    private final int count;

    public Simple(int count) {
        this.count = count;
    }

    public int doubled() {
        return count * 2;
    }
}
