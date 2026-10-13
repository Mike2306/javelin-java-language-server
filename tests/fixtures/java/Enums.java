// Enum with constants, a constant body, and ordinary members.
package com.example;

public enum Enums {
    FIRST,
    SECOND(2),
    THIRD {
        @Override
        int weight() {
            return 3;
        }
    };

    private final int value;

    Enums() {
        this(0);
    }

    Enums(int value) {
        this.value = value;
    }

    int weight() {
        return value;
    }
}
