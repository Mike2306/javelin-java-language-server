package com.acme;

import java.util.List;

/** Generic, nested, anonymous, local, lambda, varargs, private and static members. */
public class Box<T extends Comparable<T>> implements java.io.Serializable {
    public static final int LIMIT = 3;
    private int hidden;
    protected String label;
    public List<String>[] shelves;

    public Box() {}

    public T get() { return null; }

    public List<String> names() { return null; }

    public void put(T value, int[] counts) {}

    public static Box<String> of(String... parts) { return null; }

    public <R> R map(java.util.function.Function<T, R> f) { return null; }

    /** A method's own `T`, which shadows the class's. */
    public <T> T shadow(T value) { return value; }

    private void secret() {}

    public Runnable task() {
        return new Runnable() {
            public void run() {}
        };
    }

    void local() {
        class Local {}
        new Local();
    }

    public Runnable lambda() {
        return () -> {};
    }

    public static class Entry {
        public String key() { return null; }
    }

    public class Inner {
        public class Deeper {}
    }
}
