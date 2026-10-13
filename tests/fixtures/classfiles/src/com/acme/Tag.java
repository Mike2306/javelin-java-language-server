package com.acme;

public @interface Tag {
    String value();

    int weight() default 1;
}
