// A field, a parameter, a block-local and a lambda parameter, all called
// `value`. Deliberately not valid Java — a local may not shadow a parameter of
// the same method — but the scope tree is syntax, and the point is that four
// bindings of one name at four nesting depths each win in their own region.
package com.example;

import java.util.function.IntUnaryOperator;

public class Shadowing {
    int inMethod(int value) {
        int fromParameter = value;
        {
            int value = 3;
            int fromBlock = value;
        }
        return fromParameter;
    }

    IntUnaryOperator inLambda() {
        return value -> value + 1;
    }

    int inAnotherMethod() {
        return value;
    }

    // Declared last on purpose: a field is visible above its own declaration,
    // and every reference to `value` above depends on that being true.
    private int value = 1;
}
