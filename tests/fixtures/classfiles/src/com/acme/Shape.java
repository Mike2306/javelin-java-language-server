package com.acme;

public interface Shape {
    int SIDES = 0;

    double area();

    static Shape unit() { return null; }

    default String describe() { return null; }
}
