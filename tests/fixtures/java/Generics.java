// Generic class, generic method, bounded and wildcard type parameters.
package com.example;

public class Generics<T extends Comparable<T>, U> {
    private final java.util.List<? extends T> items;

    public Generics(java.util.List<? extends T> items) {
        this.items = items;
    }

    public <R> java.util.Map<T, R> index(java.util.function.Function<? super T, R> f) {
        java.util.Map<T, R> out = new java.util.HashMap<>();
        return out;
    }

    static <E> void copy(java.util.List<? super E> target, E[] source) {
    }
}
