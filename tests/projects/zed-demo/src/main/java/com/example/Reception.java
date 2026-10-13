package com.example;

/**
 * A second file in the same package, so that a name written in one file can
 * mean a type declared in another.
 *
 * <p>Nothing in the pass opens {@code Greeter.java} to answer a question asked
 * here: the workspace source index is what turns {@code Greeter} below into
 * {@code com.example.Greeter}, and the index is what says where that is
 * written.
 */
public class Reception {
    private final Greeter greeter;

    Reception(Greeter greeter) {
        this.greeter = greeter;
    }

    public String welcome() {
        return greeter.greet();
    }
}
