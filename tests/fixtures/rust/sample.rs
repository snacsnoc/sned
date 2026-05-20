/// Sample Rust fixture for tree-sitter parsing tests

pub struct RustStruct {
    pub value: i32,
}

impl RustStruct {
    pub fn get_value(&self) -> i32 {
        self.value
    }
}

pub fn rust_main() {
    let s = RustStruct { value: 42 };
    println!("{}", s.get_value());
}

fn helper_function() {
    // Helper function
}
