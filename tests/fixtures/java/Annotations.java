// An annotation-type declaration and members carrying annotations.
package com.example;

public class Annotations {
    @interface Marker {
        String value();

        int count() default 1;
    }

    @Marker("field")
    private int annotated;

    @Marker(value = "method", count = 2)
    @SafeVarargs
    final void go(@Marker("param") String... rest) {
    }
}
