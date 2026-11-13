//! The generated module: imports `example:greeter/greeter` and exports
//! `example:greeter-wrapped/greeter`, the same interface with every return
//! type wrapped in `result<T, invocation-error>`.

module_gen_macro::wrap_in_result!(path = "../wit", world = "inner");
