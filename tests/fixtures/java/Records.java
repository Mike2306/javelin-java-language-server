// Record with components, a compact constructor, and a nested record.
package com.example;

public record Records(int x, String label) {
    public Records {
        label = label.trim();
    }

    public Records(int x) {
        this(x, "");
    }

    public int doubled() {
        return x * 2;
    }

    record Nested(double a, double b) {
    }
}
