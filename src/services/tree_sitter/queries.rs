//! Tree-sitter query definitions for supported languages.
//!
//! These queries are ported from `dirac/src/services/tree-sitter/queries/`
//! and use standard tree-sitter query syntax.

/// C language query
pub const C_QUERY: &str = r#"
;; Structs (Classes)
(
  (comment)* @doc
  .
  (struct_specifier
    name: (type_identifier) @name.definition.class
    body: (_)) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Unions (Classes)
(
  (comment)* @doc
  .
  (declaration
    type: (union_specifier
      name: (type_identifier) @name.definition.class)) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Enums (Classes)
(
  (comment)* @doc
  .
  (enum_specifier
    name: (type_identifier) @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Functions
(
  (comment)* @doc
  .
  (function_definition
    declarator: [
      (function_declarator
        declarator: (identifier) @name.definition.function)
      (pointer_declarator
        declarator: (function_declarator
          declarator: (identifier) @name.definition.function))
    ]
  ) @definition.function
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.function)
)

;; Function Prototypes
(
  (comment)* @doc
  .
  (declaration
    declarator: [
      (function_declarator
        declarator: (identifier) @name.definition.function)
      (pointer_declarator
        declarator: (function_declarator
          declarator: (identifier) @name.definition.function))
    ]
  ) @definition.function
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.function)
)

;; Typedefs
(
  (comment)* @doc
  .
  (type_definition
    declarator: (type_identifier) @name.definition.type) @definition.type
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.type)
)

;; References
(identifier) @name.reference
(type_identifier) @name.reference
"#;

/// C# language query
pub const CSHARP_QUERY: &str = r#"
;; Namespaces
(
  (comment)* @doc
  .
  (namespace_declaration
    name: [(identifier) (qualified_name)] @name.definition.module) @definition.module
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.module)
)

;; Classes
(
  (comment)* @doc
  .
  (class_declaration
    name: (identifier) @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Interfaces
(
  (comment)* @doc
  .
  (interface_declaration
    name: (identifier) @name.definition.interface) @definition.interface
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.interface)
)

;; Enums
(
  (comment)* @doc
  .
  (enum_declaration
    name: (identifier) @name.definition.enum) @definition.enum
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.enum)
)

;; Structs (Classes)
(
  (comment)* @doc
  .
  (struct_declaration
    name: (identifier) @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Delegates (Functions)
(
  (comment)* @doc
  .
  (delegate_declaration
    name: (identifier) @name.definition.function) @definition.function
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.function)
)

;; Records (Classes)
(
  (comment)* @doc
  .
  (record_declaration
    name: (identifier) @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Methods
(
  (comment)* @doc
  .
  (method_declaration
    name: (identifier) @name.definition.method) @definition.method
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.method)
)

;; References
(identifier) @name.reference
"#;

/// C++ language query
pub const CPP_QUERY: &str = r#"
;; Namespaces
(
  (comment)* @doc
  .
  (namespace_definition
    name: [(namespace_identifier) (nested_namespace_specifier)] @name.definition.module) @definition.module
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.module)
)

;; Classes
(
  (comment)* @doc
  .
  (class_specifier
    name: [(type_identifier) (qualified_identifier)] @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Structs (Classes)
(
  (comment)* @doc
  .
  (struct_specifier
    name: [(type_identifier) (qualified_identifier)] @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Enums (Classes)
(
  (comment)* @doc
  .
  (enum_specifier
    name: [(type_identifier) (qualified_identifier)] @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Unions (Classes)
(
  (comment)* @doc
  .
  (union_specifier
    name: [(type_identifier) (qualified_identifier)] @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Functions and Methods
(
  (comment)* @doc
  .
  (function_definition
    declarator: [
      (function_declarator
        declarator: [
          (identifier) @name.definition.function
          (field_identifier) @name.definition.method
          (qualified_identifier) @name.definition.method
          (destructor_name) @name.definition.method
          (operator_name) @name.definition.method
        ]
      )
      (pointer_declarator
        declarator: (function_declarator
          declarator: [
            (identifier) @name.definition.function
            (field_identifier) @name.definition.method
            (qualified_identifier) @name.definition.method
            (destructor_name) @name.definition.method
            (operator_name) @name.definition.method
          ]
        )
      )
      (reference_declarator
        (function_declarator
          declarator: [
            (identifier) @name.definition.function
            (field_identifier) @name.definition.method
            (qualified_identifier) @name.definition.method
            (destructor_name) @name.definition.method
            (operator_name) @name.definition.method
          ]
        )
      )
    ]
  ) @definition.symbol
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.symbol)
)

;; Map @definition.symbol to specific kinds
((function_definition declarator: [
  (function_declarator declarator: (identifier))
  (pointer_declarator declarator: (function_declarator declarator: (identifier)))
  (reference_declarator (function_declarator declarator: (identifier)))
]) @definition.function)

((function_definition declarator: [
  (function_declarator declarator: [(field_identifier) (qualified_identifier) (destructor_name) (operator_name)])
  (pointer_declarator declarator: (function_declarator declarator: [(field_identifier) (qualified_identifier) (destructor_name) (operator_name)]))
  (reference_declarator (function_declarator declarator: [(field_identifier) (qualified_identifier) (destructor_name) (operator_name)]))
]) @definition.method)

;; Function Prototypes
(
  (comment)* @doc
  .
  (declaration
    (function_declarator
      declarator: [
        (identifier) @name.definition.function
        (field_identifier) @name.definition.method
        (qualified_identifier) @name.definition.method
      ]
    )
  ) @definition.symbol
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.symbol)
)

;; Map prototypes to specific kinds
((declaration (function_declarator declarator: (identifier))) @definition.function)
((declaration (function_declarator declarator: (field_identifier))) @definition.method)
((declaration (function_declarator declarator: (qualified_identifier))) @definition.method)

;; Lambdas in field initializers (dispatch tables)
(initializer_pair
  designator: (field_designator (field_identifier) @name.definition.method)
  value: (lambda_expression)) @definition.method

;; Lambdas assigned to variables
(declaration
  declarator: (init_declarator
    declarator: (identifier) @name.definition.function
    value: (lambda_expression))) @definition.function

;; Typedefs
(
  (comment)* @doc
  .
  (type_definition
    declarator: [
      (type_identifier) @name.definition.type
      (identifier) @name.definition.type
    ]
  ) @definition.type
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.type)
)

;; References
(identifier) @name.reference
(field_identifier) @name.reference
(type_identifier) @name.reference
(namespace_identifier) @name.reference
"#;

/// Go language query
pub const GO_QUERY: &str = r#"
;; Package
(
  (comment)* @doc
  .
  (package_clause
    (package_identifier) @name.definition.module) @definition.module
  (#strip! @doc "^//\\s*")
  (#select-adjacent! @doc @definition.module)
)

;; Functions
(
  (comment)* @doc
  .
  (function_declaration
    name: (identifier) @name.definition.function) @definition.function
  (#strip! @doc "^//\\s*")
  (#select-adjacent! @doc @definition.function)
)

;; Methods
(
  (comment)* @doc
  .
  (method_declaration
    name: [(field_identifier) (identifier)] @name.definition.method) @definition.method
  (#strip! @doc "^//\\s*")
  (#select-adjacent! @doc @definition.method)
)

;; Structs (Classes)
(
  (comment)* @doc
  .
  (type_spec
    name: (type_identifier) @name.definition.class
    type: (struct_type)) @definition.class
  (#strip! @doc "^//\\s*")
  (#select-adjacent! @doc @definition.class)
)

;; Interfaces
(
  (comment)* @doc
  .
  (type_spec
    name: (type_identifier) @name.definition.interface
    type: (interface_type)) @definition.interface
  (#strip! @doc "^//\\s*")
  (#select-adjacent! @doc @definition.interface)
)

;; Type Aliases
(
  (comment)* @doc
  .
  (type_spec
    name: (type_identifier) @name.definition.type) @definition.type
  (#strip! @doc "^//\\s*")
  (#select-adjacent! @doc @definition.type)
)

;; Func literals in keyed elements (dispatch tables)
(keyed_element
  (literal_element) @name.definition.method
  (literal_element (func_literal))) @definition.method

;; Short variable declarations for functions
(short_var_declaration
  left: (expression_list (identifier) @name.definition.function)
  right: (expression_list (func_literal))) @definition.function

;; References
(identifier) @name.reference
(field_identifier) @name.reference
(type_identifier) @name.reference
(package_identifier) @name.reference
"#;

/// Java language query
pub const JAVA_QUERY: &str = r#"
;; Package
(
  [(line_comment) (block_comment)]* @doc
  .
  (package_declaration
    (scoped_identifier) @name.definition.module) @definition.module
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.module)
)

;; Classes
(
  [(line_comment) (block_comment)]* @doc
  .
  (class_declaration
    name: (identifier) @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Interfaces
(
  [(line_comment) (block_comment)]* @doc
  .
  (interface_declaration
    name: (identifier) @name.definition.interface) @definition.interface
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.interface)
)

;; Enums
(
  [(line_comment) (block_comment)]* @doc
  .
  (enum_declaration
    name: (identifier) @name.definition.enum) @definition.enum
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.enum)
)

;; Records (Classes)
(
  [(line_comment) (block_comment)]* @doc
  .
  (record_declaration
    name: (identifier) @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Annotation Types (Interfaces)
(
  [(line_comment) (block_comment)]* @doc
  .
  (annotation_type_declaration
    name: (identifier) @name.definition.interface) @definition.interface
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.interface)
)

;; Methods
(
  [(line_comment) (block_comment)]* @doc
  .
  (method_declaration
    name: (identifier) @name.definition.method) @definition.method
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.method)
)

;; References
(identifier) @name.reference
(type_identifier) @name.reference
"#;

/// JavaScript/JSX language query
pub const JAVASCRIPT_QUERY: &str = r#"
;; Methods
(
  (comment)* @doc
  .
  (method_definition
    name: [(property_identifier) (identifier)] @name.definition.method) @definition.method
  (#not-eq? @name.definition.method "constructor")
  (#strip! @doc "^[\s\*/]+|[\s\*/]+$")
  (#select-adjacent! @doc @definition.method)
)

;; Classes
(
  (comment)* @doc
  .
  [
    (class
      name: (_) @name.definition.class)
    (class_declaration
      name: (_) @name.definition.class)
  ] @definition.class
  (#strip! @doc "^[\s\*/]+|[\s\*/]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Functions
(
  (comment)* @doc
  .
  [
    (function_declaration
      name: (identifier) @name.definition.function)
    (generator_function_declaration
      name: (identifier) @name.definition.function)
  ] @definition.function
  (#strip! @doc "^[\s\*/]+|[\s\*/]+$")
  (#select-adjacent! @doc @definition.function)
)

;; Object properties with arrow functions
(
  (comment)* @doc
  .
  (pair
    key: [(property_identifier) (identifier)] @name.definition.method
    value: [(arrow_function) (function_expression)]) @definition.method
  (#strip! @doc "^[\s\*/]+|[\s\*/]+$")
  (#select-adjacent! @doc @definition.method)
)

;; Variable declarations with arrow functions
(
  (comment)* @doc
  .
  [
    (lexical_declaration
      (variable_declarator
        name: (identifier) @name.definition.function
        value: [(arrow_function) (function_expression)]))
    (variable_declaration
      (variable_declarator
        name: (identifier) @name.definition.function
        value: [(arrow_function) (function_expression)]))
  ] @definition.function
  (#strip! @doc "^[\s\*/]+|[\s\*/]+$")
  (#select-adjacent! @doc @definition.function)
)

;; References
(identifier) @name.reference
(property_identifier) @name.reference
"#;

/// Kotlin language query

/// PHP language query
pub const PHP_QUERY: &str = r#"
;; Namespaces
(
  (comment)* @doc
  .
  (namespace_definition
    name: (namespace_name) @name.definition.module) @definition.module
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.module)
)

;; Classes
(
  (comment)* @doc
  .
  (class_declaration
    name: (name) @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Interfaces
(
  (comment)* @doc
  .
  (interface_declaration
    name: (name) @name.definition.interface) @definition.interface
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.interface)
)

;; Traits
(
  (comment)* @doc
  .
  (trait_declaration
    name: (name) @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Functions
(
  (comment)* @doc
  .
  (function_definition
    name: (name) @name.definition.function) @definition.function
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.function)
)

;; Methods
(
  (comment)* @doc
  .
  (method_declaration
    name: (name) @name.definition.method) @definition.method
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.method)
)

;; References
(name) @name.reference
"#;

/// Python language query
pub const PYTHON_QUERY: &str = r#"
;; Classes
(class_definition
  name: (identifier) @name.definition.class
  body: (block . (expression_statement (string) @doc)?)) @definition.class

;; Methods (functions inside classes)
(class_definition
  body: (block
    (function_definition
      name: (identifier) @name.definition.method
      body: (block . (expression_statement (string) @doc)?)) @definition.method))

;; Top-level Functions
(function_definition
  name: (identifier) @name.definition.function
  body: (block . (expression_statement (string) @doc)?)) @definition.function

;; Decorated Definitions
(decorated_definition
  definition: [
    (class_definition
      name: (identifier) @name.definition.class
      body: (block . (expression_statement (string) @doc)?))
    (function_definition
      name: (identifier) @name.definition.function
      body: (block . (expression_statement (string) @doc)?))
  ]) @definition.symbol

;; Map @definition.symbol to specific kinds
((decorated_definition definition: (class_definition)) @definition.class)
((decorated_definition definition: (function_definition)) @definition.function)

;; Lambdas assigned to variables
(assignment
  left: (identifier) @name.definition.function
  right: (lambda)) @definition.function

;; Lambdas in dictionaries
(pair
  key: [(string) (identifier)] @name.definition.method
  value: (lambda)) @definition.method

;; Legacy Type Aliases
(assignment
  left: (identifier) @name.definition.type
  (#match? @name.definition.type "^[A-Z][a-zA-Z0-9_]*$")
  right: [
    (subscript)
    (identifier)
  ]) @definition.type

;; References
(identifier) @name.reference
(attribute attribute: (identifier) @name.reference)
(keyword_argument name: (identifier) @name.reference)
"#;

/// Ruby language query
pub const RUBY_QUERY: &str = r#"
;; Modules
(
  (comment)* @doc
  .
  (module
    name: (constant) @name.definition.module) @definition.module
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.module)
)

;; Classes
(
  (comment)* @doc
  .
  (class
    name: (constant) @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Methods
(
  (comment)* @doc
  .
  (method
    name: (identifier) @name.definition.method) @definition.method
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.method)
)

;; Singleton Methods
(
  (comment)* @doc
  .
  (singleton_method
    name: (identifier) @name.definition.method) @definition.method
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.method)
)

;; References
(identifier) @name.reference
(constant) @name.reference
"#;

/// Rust language query
pub const RUST_QUERY: &str = r#"
;; Modules
(
  [(line_comment) (block_comment)]* @doc
  .
  (mod_item
    name: (identifier) @name.definition.module) @definition.module
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.module)
)

;; Structs (Classes)
(
  [(line_comment) (block_comment)]* @doc
  .
  (struct_item
    name: (type_identifier) @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Enums (Classes)
(
  [(line_comment) (block_comment)]* @doc
  .
  (enum_item
    name: (type_identifier) @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Traits (Interfaces)
(
  [(line_comment) (block_comment)]* @doc
  .
  (trait_item
    name: (type_identifier) @name.definition.interface) @definition.interface
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.interface)
)

;; Type Aliases
(
  [(line_comment) (block_comment)]* @doc
  .
  (type_item
    name: (type_identifier) @name.definition.type) @definition.type
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.type)
)

;; Functions
(
  [(line_comment) (block_comment)]* @doc
  .
  (function_item
    name: (identifier) @name.definition.function) @definition.function
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.function)
)

;; Macros
(
  [(line_comment) (block_comment)]* @doc
  .
  (macro_definition
    name: (identifier) @name.definition.macro) @definition.macro
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.macro)
)

;; Implementations
(
  [(line_comment) (block_comment)]* @doc
  .
  (impl_item
    type: (type_identifier) @name.definition.class) @definition.class
  (#strip! @doc "^[/\\*!\\s]+|[\\*!\\s]+$")
  (#select-adjacent! @doc @definition.class)
)

;; References
(identifier) @name.reference
"#;

/// Swift language query
pub const SWIFT_QUERY: &str = r#"
;; Classes
(class_declaration
  name: (type_identifier) @name.definition.class) @definition.class

;; Protocols (Interfaces)
(protocol_declaration
  name: (type_identifier) @name.definition.interface) @definition.interface

;; Functions
(function_declaration
  name: (simple_identifier) @name.definition.function) @definition.function

;; Methods (functions inside class/struct/protocol)
(class_body
  [
    (function_declaration
      name: (simple_identifier) @name.definition.method)
    (init_declaration "init" @name.definition.method)
    (deinit_declaration "deinit" @name.definition.method)
  ] @definition.method)

;; Properties
(property_declaration
  (pattern (simple_identifier) @name.definition.property)) @definition.property

;; Type Aliases
(typealias_declaration
  name: (type_identifier) @name.definition.type) @definition.type

;; Closures assigned to variables
(property_declaration
  (pattern (simple_identifier) @name.definition.function)
  (lambda_literal)) @definition.function

;; References
(simple_identifier) @name.reference
(type_identifier) @name.reference
"#;

/// TypeScript/TSX language query
pub const TYPESCRIPT_QUERY: &str = r#"
;; Methods
(
  (comment)* @doc
  .
  (method_definition
    name: [(property_identifier) (identifier)] @name.definition.method) @definition.method
  (#not-eq? @name.definition.method "constructor")
  (#strip! @doc "^[\\s\\*/]+|[\\s\\*/]+$")
  (#select-adjacent! @doc @definition.method)
)

;; Classes
(
  (comment)* @doc
  .
  [
    (class
      name: (_) @name.definition.class)
    (class_declaration
      name: (_) @name.definition.class)
  ] @definition.class
  (#strip! @doc "^[\\s\\*/]+|[\\s\\*/]+$")
  (#select-adjacent! @doc @definition.class)
)

;; Interfaces
(
  (comment)* @doc
  .
  (interface_declaration
    name: (type_identifier) @name.definition.interface) @definition.interface
  (#strip! @doc "^[\\s\\*/]+|[\\s\\*/]+$")
  (#select-adjacent! @doc @definition.interface)
)

;; Functions
(
  (comment)* @doc
  .
  [
    (function_declaration
      name: (identifier) @name.definition.function)
    (generator_function_declaration
      name: (identifier) @name.definition.function)
  ] @definition.function
  (#strip! @doc "^[\\s\\*/]+|[\\s\\*/]+$")
  (#select-adjacent! @doc @definition.function)
)

;; Object properties with arrow functions
(
  (comment)* @doc
  .
  (pair
    key: [(property_identifier) (identifier)] @name.definition.method
    value: [(arrow_function) (function_expression)]) @definition.method
  (#strip! @doc "^[\\s\\*/]+|[\\s\\*/]+$")
  (#select-adjacent! @doc @definition.method)
)

;; Class properties with arrow functions
(
  (comment)* @doc
  .
  [
    (method_definition
      name: [(property_identifier) (identifier)] @name.definition.method
      value: [(arrow_function) (function_expression)])
  ] @definition.method
  (#strip! @doc "^[\\s\\*/]+|[\\s\\*/]+$")
  (#select-adjacent! @doc @definition.method)
)

;; Variable declarations with arrow functions
(
  (comment)* @doc
  .
  [
    (lexical_declaration
      (variable_declarator
        name: (identifier) @name.definition.function
        value: [(arrow_function) (function_expression)]))
    (variable_declaration
      (variable_declarator
        name: (identifier) @name.definition.function
        value: [(arrow_function) (function_expression)]))
  ] @definition.function
  (#strip! @doc "^[\\s\\*/]+|[\\s\\*/]+$")
  (#select-adjacent! @doc @definition.function)
)

;; Type aliases
(type_alias_declaration
  name: (type_identifier) @name.definition.type) @definition.type

;; References
(identifier) @name.reference
(type_identifier) @name.reference
(property_identifier) @name.reference
"#;
