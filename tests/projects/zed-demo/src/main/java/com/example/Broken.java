package com.example;

// Deliberately unparseable. Open this to confirm that error recovery degrades
// one file rather than the session: the server must keep serving Greeter.java,
// and the log must show `recovered=true` rather than a crash.
public class Broken {
    public void go() {
        int x = ;
