package com.acme;

/** `compareTo(Square)` makes javac emit a synthetic bridge `compareTo(Object)`. */
public class Square implements Shape, Comparable<Square> {
    public double area() { return 0; }

    public int compareTo(Square other) { return 0; }

    /** No `Signature`: a descriptor's `Object` that may be an erased type variable. */
    public Object any() { return null; }
}
