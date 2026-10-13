// Nested static class, inner class, local class, and anonymous class.
package com.example;

public class Nested {
    static class Static {
        int inStatic;
    }

    class Inner {
        int inInner;
    }

    void go() {
        class Local {
            int inLocal;
        }
        Runnable anonymous = new Runnable() {
            @Override
            public void run() {
                int inAnonymous = 1;
            }
        };
    }
}
