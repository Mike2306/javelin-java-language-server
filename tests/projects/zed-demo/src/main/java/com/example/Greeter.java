package com.example;

import java.util.List;
import java.util.Map;

/**
 * The file to poke at during manual Zed testing.
 *
 * <p>Deliberately covers the constructs each phase is expected to handle:
 * generics, lambdas, records, nested types, shadowed locals, a {@code var}
 * local, an unqualified call, and non-ASCII content for UTF-16 position mapping.
 */
public class Greeter {
    private static final String DEFAULT = "Welt";

    private final String name;
    private final Map<String, Integer> seen;

    public Greeter(String name, Map<String, Integer> seen) {
        this.name = name;
        this.seen = seen;
    }

    /** Non-ASCII on purpose: 🚀 is two UTF-16 code units, ä is one. */
    public String greet() {
        String greeting = "Grüße, " + name + " 🚀";
        return greeting + " (" + timesSeen() + ")";
    }

    public List<String> greetAll(List<String> names) {
        // `name` here shadows the field, which local scope resolution must respect.
        return names.stream().map(name -> "Hallo, " + name).toList();
    }

    public int timesSeen() {
        return seen.getOrDefault(name, 0);
    }

    public static void main(String[] args) {
        var greeter = new Greeter(DEFAULT, Map.of(DEFAULT, 1));
        System.out.println(greeter.greet());
    }

    /** A record, to check that newer syntax parses. */
    public record Visit(String who, int count) {
        public Visit {
            if (count < 0) {
                throw new IllegalArgumentException("count must not be negative");
            }
        }
    }

    /** A nested interface, to check nesting shows up in the outline. */
    interface Listener {
        void onGreet(String message);
    }
}
