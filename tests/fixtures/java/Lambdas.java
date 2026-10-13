// Lambdas with and without explicit parameter types, and method references.
package com.example;

import java.util.List;
import java.util.function.BiFunction;

public class Lambdas {
    private final Runnable noop = () -> {};

    void run(List<String> names) {
        BiFunction<Integer, Integer, Integer> add = (Integer a, Integer b) -> a + b;
        names.forEach(name -> {
            String shouted = name.toUpperCase();
            System.out.println(shouted);
        });
        names.sort(String::compareTo);
        Runnable reference = this::run2;
    }

    void run2() {
    }
}
