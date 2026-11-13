//! The plain module: a hand-written implementation of `example:greeter/greeter`.

wit_bindgen::generate!({ path: "../wit", world: "inner" });

use exports::example::greeter::greeter::Guest;

struct Inner;

impl Guest for Inner {
    fn motto() -> String {
        // Read from the `@dist_gen(...)` annotation on `motto` at compile time.
        module_gen_macro::dist_gen_str!("../wit", "greeter", "motto").to_string()
    }

    fn greet(name: String) -> String {
        format!("Hello, {name}!")
    }

    fn loaves_for(customers: u32) -> u32 {
        customers * 2
    }
}

export!(Inner);
