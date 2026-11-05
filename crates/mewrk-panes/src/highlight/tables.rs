//! Generated once from Mewrk's `codeHighlight.ts` and `fileViewers.ts`, then kept here as data.
//! Every keyword list is sorted by bytes, because `is_keyword` searches it by bisection.

use super::*;

pub(super) const BATCH_KEYWORDS: &[&str] = &[
    "CALL", "CD", "CHDIR", "CLS", "COPY", "DEFINED", "DEL", "DO", "ECHO", "ELSE", "ENABLEDELAYEDEXPANSION",
    "ENDLOCAL", "EQU", "ERRORLEVEL", "EXIST", "EXIT", "FOR", "GEQ", "GOTO", "GTR", "IF", "IN", "LEQ", "LSS", "MD",
    "MKDIR", "MOVE", "NEQ", "NOT", "NUL", "OFF", "ON", "PAUSE", "RD", "RMDIR", "SET", "SETLOCAL", "SHIFT", "START",
    "TITLE", "TYPE",
];

pub(super) const CMAKE_KEYWORDS: &[&str] = &[
    "ADD_CUSTOM_COMMAND", "ADD_CUSTOM_TARGET", "ADD_EXECUTABLE", "ADD_LIBRARY", "ADD_SUBDIRECTORY",
    "CMAKE_MINIMUM_REQUIRED", "CONFIGURE_FILE", "ELSE", "ELSEIF", "ENDFOREACH", "ENDFUNCTION", "ENDIF", "ENDMACRO",
    "ENDWHILE", "FILE", "FIND_PACKAGE", "FOREACH", "FUNCTION", "GET_FILENAME_COMPONENT", "IF", "INCLUDE",
    "INCLUDE_DIRECTORIES", "INSTALL", "LINK_DIRECTORIES", "LIST", "MACRO", "MESSAGE", "OPTION", "PROJECT", "RETURN",
    "SET", "STRING", "TARGET_COMPILE_DEFINITIONS", "TARGET_COMPILE_OPTIONS", "TARGET_INCLUDE_DIRECTORIES",
    "TARGET_LINK_LIBRARIES", "UNSET", "WHILE",
];

pub(super) const CSHARP_KEYWORDS: &[&str] = &[
    "abstract", "as", "async", "await", "base", "bool", "break", "byte", "case", "catch", "char", "checked", "class",
    "const", "continue", "decimal", "default", "delegate", "do", "double", "dynamic", "else", "enum", "event",
    "explicit", "extern", "false", "finally", "fixed", "float", "for", "foreach", "get", "goto", "if", "implicit",
    "in", "init", "int", "interface", "internal", "is", "lock", "long", "namespace", "new", "null", "object",
    "operator", "out", "override", "params", "partial", "private", "protected", "public", "readonly", "record", "ref",
    "required", "return", "sbyte", "sealed", "set", "short", "sizeof", "stackalloc", "static", "string", "struct",
    "switch", "this", "throw", "true", "try", "typeof", "uint", "ulong", "unchecked", "unsafe", "ushort", "using",
    "var", "virtual", "void", "volatile", "when", "where", "while", "with", "yield",
];

pub(super) const CSS_KEYWORDS: &[&str] = &[
    "!important", "@apply", "@charset", "@container", "@each", "@else", "@extend", "@font-face", "@forward",
    "@function", "@if", "@import", "@include", "@keyframes", "@layer", "@media", "@mixin", "@namespace", "@page",
    "@property", "@return", "@supports", "@tailwind", "@use", "auto", "from", "inherit", "initial", "none", "revert",
    "to", "unset",
];

pub(super) const C_KEYWORDS: &[&str] = &[
    "NULL", "_Atomic", "_Bool", "_Complex", "_Generic", "_Noreturn", "alignas", "alignof", "auto", "bool", "break",
    "case", "catch", "char", "char16_t", "char32_t", "char8_t", "class", "co_await", "co_return", "co_yield",
    "concept", "const", "consteval", "constexpr", "constinit", "continue", "decltype", "default", "delete", "do",
    "double", "dynamic_cast", "else", "enum", "explicit", "export", "extern", "false", "final", "float", "for",
    "friend", "goto", "if", "inline", "int", "int16_t", "int32_t", "int64_t", "int8_t", "intptr_t", "long", "mutable",
    "namespace", "new", "noexcept", "nullptr", "operator", "override", "private", "protected", "public", "register",
    "reinterpret_cast", "requires", "restrict", "return", "short", "signed", "size_t", "sizeof", "ssize_t", "static",
    "static_assert", "static_cast", "struct", "switch", "template", "this", "thread_local", "throw", "true", "try",
    "typedef", "typeid", "typename", "uint16_t", "uint32_t", "uint64_t", "uint8_t", "uintptr_t", "union", "unsigned",
    "using", "virtual", "void", "volatile", "wchar_t", "while",
];

pub(super) const DART_KEYWORDS: &[&str] = &[
    "abstract", "as", "assert", "async", "await", "base", "bool", "break", "case", "catch", "class", "const",
    "continue", "covariant", "default", "deferred", "do", "double", "dynamic", "else", "enum", "export", "extends",
    "extension", "external", "factory", "false", "final", "finally", "for", "get", "hide", "if", "implements",
    "import", "in", "int", "interface", "is", "late", "library", "mixin", "new", "null", "num", "on", "operator",
    "part", "required", "rethrow", "return", "sealed", "set", "show", "static", "super", "switch", "sync", "this",
    "throw", "true", "try", "typedef", "var", "void", "when", "while", "with", "yield",
];

pub(super) const DOCKER_KEYWORDS: &[&str] = &[
    "ADD", "ARG", "AS", "CMD", "COPY", "ENTRYPOINT", "ENV", "EXPOSE", "FROM", "HEALTHCHECK", "LABEL", "MAINTAINER",
    "ONBUILD", "RUN", "SHELL", "STOPSIGNAL", "USER", "VOLUME", "WORKDIR",
];

pub(super) const ELIXIR_KEYWORDS: &[&str] = &[
    "after", "alias", "and", "case", "catch", "cond", "def", "defdelegate", "defexception", "defguard", "defimpl",
    "defmacro", "defmodule", "defp", "defprotocol", "defstruct", "do", "else", "end", "false", "fn", "for", "if",
    "import", "in", "nil", "not", "or", "quote", "raise", "receive", "require", "rescue", "true", "try", "unless",
    "unquote", "use", "when", "with",
];

pub(super) const ELM_KEYWORDS: &[&str] = &[
    "alias", "as", "case", "else", "exposing", "if", "import", "in", "let", "module", "of", "port", "then", "type",
];

pub(super) const ERLANG_KEYWORDS: &[&str] = &[
    "after", "and", "andalso", "band", "begin", "bnot", "bor", "bsl", "bsr", "bxor", "case", "catch", "cond", "div",
    "end", "false", "fun", "if", "let", "not", "of", "or", "orelse", "receive", "rem", "true", "try", "when", "xor",
];

pub(super) const FORTRAN_KEYWORDS: &[&str] = &[
    "ALLOCATABLE", "ALLOCATE", "CALL", "CASE", "CHARACTER", "COMPLEX", "CONTAINS", "CYCLE", "DEALLOCATE", "DEFAULT",
    "DIMENSION", "DO", "DOUBLE", "ELEMENTAL", "ELSE", "ELSEIF", "END", "ENDDO", "ENDIF", "EXIT", "FUNCTION", "IF",
    "IMPLICIT", "IN", "INOUT", "INTEGER", "INTENT", "INTERFACE", "LOGICAL", "MODULE", "NONE", "OUT", "PARAMETER",
    "PRECISION", "PRINT", "PRIVATE", "PROCEDURE", "PROGRAM", "PUBLIC", "PURE", "READ", "REAL", "RECURSIVE", "RESULT",
    "RETURN", "SELECT", "STOP", "SUBROUTINE", "THEN", "TYPE", "USE", "WHILE", "WRITE",
];

pub(super) const GLEAM_KEYWORDS: &[&str] = &[
    "False", "Nil", "True", "as", "assert", "case", "const", "external", "fn", "if", "import", "let", "opaque",
    "panic", "pub", "todo", "try", "type", "use",
];

pub(super) const GO_KEYWORDS: &[&str] = &[
    "any", "bool", "break", "byte", "case", "chan", "comparable", "complex128", "complex64", "const", "continue",
    "default", "defer", "else", "error", "fallthrough", "false", "float32", "float64", "for", "func", "go", "goto",
    "if", "import", "int", "int16", "int32", "int64", "int8", "interface", "iota", "map", "nil", "package", "range",
    "return", "rune", "select", "string", "struct", "switch", "true", "type", "uint", "uint16", "uint32", "uint64",
    "uint8", "uintptr", "var",
];

pub(super) const GRAPHQL_KEYWORDS: &[&str] = &[
    "directive", "enum", "extend", "false", "fragment", "implements", "import", "input", "interface", "map",
    "message", "mutation", "null", "on", "oneof", "option", "optional", "package", "query", "repeated", "required",
    "reserved", "returns", "rpc", "scalar", "schema", "service", "stream", "subscription", "syntax", "true", "type",
    "union",
];

pub(super) const GROOVY_KEYWORDS: &[&str] = &[
    "abstract", "as", "assert", "boolean", "break", "byte", "case", "catch", "char", "class", "const", "continue",
    "def", "default", "do", "double", "else", "enum", "extends", "false", "final", "finally", "float", "for", "goto",
    "if", "implements", "import", "in", "instanceof", "int", "interface", "it", "long", "native", "new", "non-sealed",
    "null", "package", "permits", "private", "protected", "public", "record", "return", "sealed", "short", "static",
    "strictfp", "super", "switch", "synchronized", "this", "throw", "throws", "trait", "transient", "true", "try",
    "var", "void", "volatile", "while", "yield",
];

pub(super) const HASKELL_KEYWORDS: &[&str] = &[
    "as", "case", "class", "data", "default", "deriving", "do", "else", "family", "forall", "foreign", "hiding", "if",
    "import", "in", "infix", "infixl", "infixr", "instance", "let", "mdo", "module", "newtype", "of", "proc",
    "qualified", "rec", "then", "type", "where",
];

pub(super) const HCL_KEYWORDS: &[&str] = &[
    "content", "count", "data", "depends_on", "dynamic", "false", "for", "for_each", "if", "in", "lifecycle",
    "locals", "module", "null", "output", "provider", "resource", "terraform", "true", "variable",
];

pub(super) const INI_KEYWORDS: &[&str] = &[
    "false", "no", "off", "on", "true", "yes",
];

pub(super) const JAVA_KEYWORDS: &[&str] = &[
    "abstract", "assert", "boolean", "break", "byte", "case", "catch", "char", "class", "const", "continue",
    "default", "do", "double", "else", "enum", "extends", "false", "final", "finally", "float", "for", "goto", "if",
    "implements", "import", "instanceof", "int", "interface", "long", "native", "new", "non-sealed", "null",
    "package", "permits", "private", "protected", "public", "record", "return", "sealed", "short", "static",
    "strictfp", "super", "switch", "synchronized", "this", "throw", "throws", "transient", "true", "try", "var",
    "void", "volatile", "while", "yield",
];

pub(super) const JSON_KEYWORDS: &[&str] = &[
    "false", "null", "true",
];

pub(super) const JS_KEYWORDS: &[&str] = &[
    "abstract", "accessor", "any", "as", "async", "await", "bigint", "boolean", "break", "case", "catch", "class",
    "const", "continue", "debugger", "declare", "default", "delete", "do", "else", "enum", "export", "extends",
    "false", "finally", "for", "from", "function", "get", "if", "implements", "import", "in", "infer", "instanceof",
    "interface", "keyof", "let", "namespace", "never", "new", "null", "number", "object", "of", "override", "package",
    "private", "protected", "public", "readonly", "return", "satisfies", "set", "static", "string", "super", "switch",
    "symbol", "this", "throw", "true", "try", "type", "typeof", "undefined", "unknown", "var", "void", "while",
    "with", "yield",
];

pub(super) const JULIA_KEYWORDS: &[&str] = &[
    "abstract", "baremodule", "begin", "break", "catch", "const", "continue", "do", "else", "elseif", "end", "export",
    "false", "finally", "for", "function", "global", "if", "import", "in", "let", "local", "macro", "missing",
    "module", "mutable", "nothing", "primitive", "quote", "return", "struct", "true", "try", "type", "using", "where",
    "while",
];

pub(super) const KOTLIN_KEYWORDS: &[&str] = &[
    "abstract", "actual", "annotation", "as", "break", "by", "catch", "class", "companion", "const", "constructor",
    "continue", "crossinline", "data", "delegate", "do", "dynamic", "else", "enum", "expect", "external", "false",
    "final", "finally", "for", "fun", "get", "if", "import", "in", "infix", "init", "inline", "inner", "interface",
    "internal", "is", "lateinit", "noinline", "null", "object", "open", "operator", "out", "override", "package",
    "private", "protected", "public", "reified", "return", "sealed", "set", "super", "suspend", "tailrec", "this",
    "throw", "true", "try", "typealias", "val", "value", "var", "vararg", "when", "where", "while",
];

pub(super) const LISP_KEYWORDS: &[&str] = &[
    "and", "begin", "case", "catch", "cond", "condp", "def", "define", "defmacro", "defmethod", "defmulti", "defn",
    "defn-", "defonce", "defparameter", "defprotocol", "defrecord", "deftype", "defun", "defvar", "do", "doseq",
    "dotimes", "else", "false", "finally", "fn", "for", "if", "if-let", "if-not", "import", "lambda", "let", "let*",
    "letfn", "letrec", "loop", "nil", "not", "ns", "or", "progn", "quote", "recur", "require", "set!", "setf", "setq",
    "throw", "true", "try", "use", "when", "when-let", "when-not",
];

pub(super) const LOG_LEVELS: &[&str] = &[
    "CRIT", "CRITICAL", "DEBUG", "ERR", "ERROR", "FATAL", "INFO", "NOTICE", "PANIC", "TRACE", "VERBOSE", "WARN",
    "WARNING",
];

pub(super) const LUA_KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in", "local", "nil",
    "not", "or", "repeat", "return", "self", "then", "true", "until", "while",
];

pub(super) const MAKEFILE_KEYWORDS: &[&str] = &[
    ".PHONY", "define", "else", "endef", "endif", "export", "ifdef", "ifeq", "ifndef", "ifneq", "include", "override",
];

pub(super) const ML_KEYWORDS: &[&str] = &[
    "abstract", "and", "as", "assert", "async", "base", "begin", "class", "constraint", "default", "delegate", "do",
    "done", "downto", "elif", "else", "end", "exception", "extern", "external", "false", "for", "fun", "function",
    "functor", "global", "if", "in", "include", "inherit", "initializer", "inline", "interface", "internal", "lazy",
    "let", "match", "member", "method", "module", "mutable", "namespace", "new", "nonrec", "null", "object", "of",
    "open", "or", "override", "private", "public", "rec", "return", "sig", "static", "struct", "then", "to", "true",
    "try", "type", "upcast", "use", "val", "virtual", "void", "when", "while", "with", "yield",
];

pub(super) const NIM_KEYWORDS: &[&str] = &[
    "addr", "and", "as", "asm", "bind", "block", "break", "case", "cast", "concept", "const", "continue", "converter",
    "defer", "discard", "distinct", "div", "do", "elif", "else", "end", "enum", "except", "export", "false",
    "finally", "for", "from", "func", "if", "import", "in", "include", "interface", "is", "isnot", "iterator", "let",
    "macro", "method", "mixin", "mod", "nil", "not", "notin", "object", "of", "or", "out", "proc", "ptr", "raise",
    "ref", "return", "shl", "shr", "static", "template", "true", "try", "tuple", "type", "using", "var", "when",
    "while", "xor", "yield",
];

pub(super) const NIX_KEYWORDS: &[&str] = &[
    "assert", "else", "false", "if", "import", "in", "inherit", "let", "null", "or", "rec", "then", "true", "with",
];

pub(super) const OBJC_KEYWORDS: &[&str] = &[
    "BOOL", "IMP", "NO", "NULL", "Nil", "SEL", "YES", "_Atomic", "_Bool", "_Complex", "_Generic", "_Noreturn",
    "alignas", "alignof", "assign", "atomic", "auto", "bool", "break", "case", "catch", "char", "char16_t",
    "char32_t", "char8_t", "class", "co_await", "co_return", "co_yield", "concept", "const", "consteval", "constexpr",
    "constinit", "continue", "copy", "decltype", "default", "delete", "do", "double", "dynamic_cast", "else", "enum",
    "explicit", "export", "extern", "false", "final", "float", "for", "friend", "goto", "id", "if", "inline",
    "instancetype", "int", "int16_t", "int32_t", "int64_t", "int8_t", "intptr_t", "long", "mutable", "namespace",
    "new", "nil", "noexcept", "nonatomic", "nonnull", "nullable", "nullptr", "operator", "override", "private",
    "protected", "public", "readonly", "readwrite", "register", "reinterpret_cast", "requires", "restrict", "return",
    "self", "short", "signed", "size_t", "sizeof", "ssize_t", "static", "static_assert", "static_cast", "strong",
    "struct", "super", "switch", "template", "this", "thread_local", "throw", "true", "try", "typedef", "typeid",
    "typename", "uint16_t", "uint32_t", "uint64_t", "uint8_t", "uintptr_t", "union", "unsigned", "using", "virtual",
    "void", "volatile", "wchar_t", "weak", "while",
];

pub(super) const PERL_KEYWORDS: &[&str] = &[
    "and", "cmp", "defined", "die", "do", "else", "elsif", "eq", "for", "foreach", "ge", "gt", "if", "last", "le",
    "local", "lt", "my", "ne", "next", "no", "not", "or", "our", "package", "print", "redo", "require", "return",
    "say", "sub", "undef", "unless", "until", "use", "warn", "while",
];

pub(super) const PHP_KEYWORDS: &[&str] = &[
    "abstract", "and", "array", "as", "break", "callable", "case", "catch", "class", "clone", "const", "continue",
    "declare", "default", "do", "echo", "else", "elseif", "empty", "enddeclare", "endfor", "endforeach", "endif",
    "endswitch", "endwhile", "enum", "extends", "false", "final", "finally", "fn", "for", "foreach", "function",
    "global", "goto", "if", "implements", "include", "include_once", "instanceof", "insteadof", "interface", "isset",
    "list", "match", "namespace", "new", "null", "or", "parent", "print", "private", "protected", "public",
    "readonly", "require", "require_once", "return", "self", "static", "switch", "throw", "trait", "true", "try",
    "unset", "use", "var", "while", "xor", "yield",
];

pub(super) const POWERSHELL_KEYWORDS: &[&str] = &[
    "-AND", "-AS", "-CONTAINS", "-EQ", "-GE", "-GT", "-IN", "-IS", "-ISNOT", "-JOIN", "-LE", "-LIKE", "-LT", "-MATCH",
    "-NE", "-NOT", "-NOTCONTAINS", "-NOTIN", "-NOTLIKE", "-NOTMATCH", "-OR", "-REPLACE", "-SPLIT", "-XOR", "BEGIN",
    "BREAK", "CATCH", "CLASS", "CONTINUE", "DATA", "DEFINE", "DO", "DYNAMICPARAM", "ELSE", "ELSEIF", "END", "ENUM",
    "EXIT", "FILTER", "FINALLY", "FOR", "FOREACH", "FROM", "FUNCTION", "IF", "IN", "PARAM", "PROCESS", "RETURN",
    "SWITCH", "THROW", "TRAP", "TRY", "UNTIL", "USING", "VAR", "WHILE", "WORKFLOW",
];

pub(super) const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "bool", "break", "bytes", "case", "class",
    "cls", "continue", "def", "del", "dict", "elif", "else", "except", "finally", "float", "for", "from", "global",
    "if", "import", "in", "int", "is", "lambda", "list", "match", "nonlocal", "not", "object", "or", "pass", "raise",
    "return", "self", "set", "str", "try", "tuple", "type", "while", "with", "yield",
];

pub(super) const RUBY_KEYWORDS: &[&str] = &[
    "alias", "and", "attr_accessor", "attr_reader", "attr_writer", "begin", "break", "case", "class", "def",
    "defined?", "do", "else", "elsif", "end", "ensure", "extend", "false", "for", "if", "in", "include", "lambda",
    "module", "next", "nil", "not", "or", "private", "proc", "protected", "public", "redo", "require",
    "require_relative", "rescue", "retry", "return", "self", "super", "then", "true", "undef", "unless", "until",
    "when", "while", "yield",
];

pub(super) const RUST_KEYWORDS: &[&str] = &[
    "Self", "as", "async", "await", "bool", "break", "char", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "f32", "f64", "false", "fn", "for", "i128", "i16", "i32", "i64", "i8", "if", "impl", "in", "isize",
    "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "static", "str", "struct", "super",
    "trait", "true", "type", "u128", "u16", "u32", "u64", "u8", "union", "unsafe", "use", "usize", "where", "while",
];

pub(super) const R_KEYWORDS: &[&str] = &[
    "FALSE", "Inf", "NA", "NA_character_", "NA_integer_", "NA_real_", "NULL", "NaN", "TRUE", "break", "else", "for",
    "function", "if", "in", "library", "next", "repeat", "require", "return", "while",
];

pub(super) const SCALA_KEYWORDS: &[&str] = &[
    "abstract", "case", "catch", "class", "def", "do", "else", "enum", "export", "extends", "false", "final",
    "finally", "for", "forSome", "given", "if", "implicit", "import", "lazy", "match", "new", "null", "object",
    "override", "package", "private", "protected", "return", "sealed", "super", "then", "this", "throw", "trait",
    "true", "try", "type", "using", "val", "var", "while", "with", "yield",
];

pub(super) const SHADER_KEYWORDS: &[&str] = &[
    "NULL", "SamplerState", "Texture2D", "_Atomic", "_Bool", "_Complex", "_Generic", "_Noreturn", "__constant__",
    "__device__", "__global__", "__host__", "__shared__", "alignas", "alignof", "attribute", "auto", "bool", "break",
    "bvec2", "bvec3", "bvec4", "case", "catch", "cbuffer", "char", "char16_t", "char32_t", "char8_t", "class",
    "co_await", "co_return", "co_yield", "concept", "const", "constant", "consteval", "constexpr", "constinit",
    "continue", "decltype", "default", "delete", "device", "discard", "do", "double", "dynamic_cast", "else", "enum",
    "explicit", "export", "extern", "f32", "false", "final", "float", "float2", "float3", "float4", "float4x4", "fn",
    "for", "friend", "goto", "half", "half2", "half3", "half4", "highp", "i32", "if", "in", "inline", "inout", "int",
    "int16_t", "int32_t", "int64_t", "int8_t", "intptr_t", "ivec2", "ivec3", "ivec4", "kernel", "layout", "let",
    "long", "lowp", "mat2", "mat3", "mat4", "mat4x4f", "mediump", "mutable", "namespace", "new", "noexcept",
    "nullptr", "operator", "out", "override", "precision", "private", "protected", "public", "register",
    "reinterpret_cast", "requires", "restrict", "return", "sampler2D", "samplerCube", "short", "signed", "size_t",
    "sizeof", "ssize_t", "static", "static_assert", "static_cast", "struct", "switch", "template", "this",
    "thread_local", "threadgroup", "throw", "true", "try", "typedef", "typeid", "typename", "u32", "uint16_t",
    "uint32_t", "uint64_t", "uint8_t", "uintptr_t", "uniform", "union", "unsigned", "using", "uvec2", "uvec3",
    "uvec4", "var", "varying", "vec2", "vec2f", "vec3", "vec3f", "vec4", "vec4f", "virtual", "void", "volatile",
    "wchar_t", "while",
];

pub(super) const SHELL_KEYWORDS: &[&str] = &[
    "alias", "break", "case", "cd", "continue", "declare", "do", "done", "echo", "elif", "else", "esac", "eval",
    "exec", "exit", "export", "false", "fi", "for", "function", "if", "in", "local", "printf", "read", "readonly",
    "return", "select", "set", "shift", "source", "test", "then", "time", "trap", "true", "typeset", "unset", "until",
    "wait", "while",
];

pub(super) const SOLIDITY_KEYWORDS: &[&str] = &[
    "abstract", "address", "assert", "bool", "break", "bytes", "bytes32", "calldata", "constant", "constructor",
    "continue", "contract", "delete", "do", "else", "emit", "enum", "error", "event", "external", "fallback", "false",
    "for", "function", "if", "immutable", "import", "int", "int256", "interface", "internal", "is", "library",
    "mapping", "memory", "modifier", "new", "override", "payable", "pragma", "private", "public", "pure", "receive",
    "require", "return", "returns", "revert", "solidity", "storage", "string", "struct", "this", "true", "uint",
    "uint256", "uint8", "unchecked", "view", "virtual", "while",
];

pub(super) const SQL_KEYWORDS: &[&str] = &[
    "ADD", "ALL", "ALTER", "AND", "ANY", "AS", "ASC", "BEGIN", "BETWEEN", "BIGINT", "BOOLEAN", "BY", "CASE", "CAST",
    "CHAR", "CHECK", "COLUMN", "COMMIT", "CONSTRAINT", "CREATE", "CROSS", "DATABASE", "DATE", "DECIMAL", "DEFAULT",
    "DELETE", "DESC", "DISTINCT", "DROP", "ELSE", "END", "EXCEPT", "EXISTS", "FALSE", "FOREIGN", "FROM", "FULL",
    "FUNCTION", "GROUP", "HAVING", "IF", "IN", "INDEX", "INNER", "INSERT", "INT", "INTEGER", "INTERSECT", "INTO",
    "IS", "JOIN", "KEY", "LEFT", "LIKE", "LIMIT", "MERGE", "NATURAL", "NOT", "NULL", "NUMERIC", "OFFSET", "ON", "OR",
    "ORDER", "OUTER", "OVER", "PARTITION", "PRIMARY", "PROCEDURE", "REAL", "REFERENCES", "REPLACE", "RETURNING",
    "RIGHT", "ROLLBACK", "ROW", "ROWS", "SCHEMA", "SELECT", "SERIAL", "SET", "SMALLINT", "TABLE", "TEXT", "THEN",
    "TIMESTAMP", "TO", "TRANSACTION", "TRIGGER", "TRUE", "TRUNCATE", "UNION", "UNIQUE", "UPDATE", "USING", "VALUES",
    "VARCHAR", "VIEW", "WHEN", "WHERE", "WINDOW", "WITH",
];

pub(super) const SWIFT_KEYWORDS: &[&str] = &[
    "Self", "actor", "any", "as", "associatedtype", "async", "await", "break", "case", "catch", "class", "continue",
    "convenience", "default", "defer", "deinit", "do", "else", "enum", "extension", "fallthrough", "false",
    "fileprivate", "final", "for", "func", "guard", "if", "import", "in", "indirect", "init", "inout", "internal",
    "is", "lazy", "let", "mutating", "nil", "none", "nonisolated", "open", "operator", "override", "private",
    "protocol", "public", "repeat", "required", "rethrows", "return", "self", "some", "static", "struct", "subscript",
    "super", "switch", "throw", "throws", "true", "try", "typealias", "var", "weak", "where", "while",
];

pub(super) const TCL_KEYWORDS: &[&str] = &[
    "catch", "else", "elseif", "error", "expr", "for", "foreach", "global", "if", "incr", "lappend", "lindex", "list",
    "llength", "namespace", "package", "proc", "puts", "return", "set", "source", "string", "switch", "upvar",
    "while",
];

pub(super) const TOML_KEYWORDS: &[&str] = &[
    "false", "true",
];

pub(super) const VB_KEYWORDS: &[&str] = &[
    "AND", "ANDALSO", "AS", "BOOLEAN", "BYREF", "BYTE", "BYVAL", "CALL", "CASE", "CATCH", "CLASS", "CONST", "DATE",
    "DIM", "DO", "DOUBLE", "EACH", "ELSE", "ELSEIF", "END", "ENUM", "ERROR", "EXIT", "FALSE", "FINALLY", "FOR",
    "FRIEND", "FUNCTION", "GET", "GOTO", "HANDLES", "IF", "IMPLEMENTS", "IMPORTS", "IN", "INHERITS", "INTEGER",
    "INTERFACE", "IS", "LET", "LIB", "LIKE", "LONG", "LOOP", "ME", "MOD", "MODULE", "MYBASE", "NAMESPACE", "NEW",
    "NEXT", "NOT", "NOTHING", "OBJECT", "OF", "ON", "OPTION", "OPTIONAL", "OR", "ORELSE", "OVERRIDES", "PARAMARRAY",
    "PRIVATE", "PROPERTY", "PROTECTED", "PUBLIC", "RAISEEVENT", "READONLY", "REDIM", "RESUME", "RETURN", "SELECT",
    "SET", "SHARED", "SHORT", "SINGLE", "STATIC", "STEP", "STOP", "STRING", "STRUCTURE", "SUB", "THEN", "THROW", "TO",
    "TRUE", "TRY", "TYPEOF", "UNTIL", "VARIANT", "WEND", "WHEN", "WHILE", "WITH", "WITHEVENTS",
];

pub(super) const VERILOG_KEYWORDS: &[&str] = &[
    "always", "always_comb", "always_ff", "always_latch", "assign", "begin", "bit", "case", "casex", "casez",
    "default", "else", "end", "endcase", "endfunction", "endgenerate", "endinterface", "endmodule", "endpackage",
    "endtask", "enum", "for", "function", "generate", "genvar", "if", "import", "initial", "inout", "input",
    "integer", "interface", "localparam", "logic", "modport", "module", "negedge", "output", "package", "parameter",
    "posedge", "reg", "struct", "task", "typedef", "while", "wire",
];

pub(super) const VHDL_KEYWORDS: &[&str] = &[
    "AFTER", "ALL", "AND", "ARCHITECTURE", "BEGIN", "BOOLEAN", "CASE", "COMPONENT", "CONSTANT", "DOWNTO", "ELSE",
    "ELSIF", "END", "ENTITY", "FALLING_EDGE", "FOR", "FUNCTION", "GENERATE", "GENERIC", "IF", "IN", "INOUT",
    "INTEGER", "IS", "LIBRARY", "LOOP", "MAP", "NAND", "NATURAL", "NOR", "NOT", "OF", "OR", "OTHERS", "OUT", "PORT",
    "PROCESS", "REPORT", "RETURN", "RISING_EDGE", "SEVERITY", "SIGNAL", "STD_LOGIC", "STD_LOGIC_VECTOR", "THEN", "TO",
    "TYPE", "UNTIL", "USE", "VARIABLE", "WAIT", "WHEN", "WHILE", "XOR",
];

pub(super) const YAML_KEYWORDS: &[&str] = &[
    "False", "Null", "True", "false", "no", "null", "off", "on", "true", "yes", "~",
];

pub(super) const ZIG_KEYWORDS: &[&str] = &[
    "addrspace", "align", "allowzero", "and", "anyerror", "anyframe", "anytype", "asm", "async", "await", "bool",
    "break", "callconv", "catch", "comptime", "const", "continue", "defer", "else", "enum", "errdefer", "error",
    "export", "extern", "f32", "f64", "false", "fn", "for", "i16", "i32", "i64", "i8", "if", "inline", "isize",
    "linksection", "noalias", "noinline", "noreturn", "nosuspend", "null", "opaque", "or", "orelse", "packed", "pub",
    "resume", "return", "struct", "suspend", "switch", "test", "threadlocal", "true", "try", "type", "u16", "u32",
    "u64", "u8", "undefined", "union", "unreachable", "usingnamespace", "usize", "var", "void", "volatile", "while",
];

/// File names, extensions and fence names, lower-cased and sorted by bytes.
pub(super) static NAMES: &[(&str, &Grammar)] = &[
    (".babelrc", &JSON),
    (".bash_logout", &SHELL),
    (".bash_profile", &SHELL),
    (".bashrc", &SHELL),
    (".clang-format", &YAML),
    (".dockerignore", &SHELL),
    (".editorconfig", &INI),
    (".env", &SHELL),
    (".eslintignore", &SHELL),
    (".eslintrc", &JSON),
    (".gitattributes", &SHELL),
    (".gitconfig", &INI),
    (".gitignore", &SHELL),
    (".gitmodules", &INI),
    (".htaccess", &SHELL),
    (".npmignore", &SHELL),
    (".npmrc", &INI),
    (".prettierignore", &SHELL),
    (".prettierrc", &JSON),
    (".profile", &SHELL),
    (".yarnrc", &INI),
    (".zprofile", &SHELL),
    (".zshenv", &SHELL),
    (".zshrc", &SHELL),
    ("asm", &ASSEMBLY),
    ("asp", &XML),
    ("aspx", &XML),
    ("assembly", &ASSEMBLY),
    ("astro", &XML),
    ("atom", &XML),
    ("awk", &SHELL),
    ("bas", &VB),
    ("bash", &SHELL),
    ("bat", &BATCH),
    ("batch", &BATCH),
    ("bats", &SHELL),
    ("bib", &LATEX),
    ("brewfile", &RUBY),
    ("bsdmakefile", &MAKEFILE),
    ("bzl", &PYTHON),
    ("c", &C),
    ("c#", &CSHARP),
    ("c++", &CPP),
    ("capfile", &RUBY),
    ("cargo.lock", &TOML),
    ("cc", &CPP),
    ("cfg", &INI),
    ("cjs", &JAVASCRIPT),
    ("clj", &LISP),
    ("cljc", &LISP),
    ("cljs", &LISP),
    ("clojure", &LISP),
    ("cls", &LATEX),
    ("cmake", &CMAKE),
    ("cmakelists.txt", &CMAKE),
    ("cmd", &BATCH),
    ("codeowners", &SHELL),
    ("command", &SHELL),
    ("comp", &SHADER),
    ("conf", &INI),
    ("console", &SHELL),
    ("containerfile", &DOCKERFILE),
    ("cpp", &CPP),
    ("cr", &RUBY),
    ("cs", &CSHARP),
    ("csharp", &CSHARP),
    ("cshtml", &XML),
    ("csproj", &XML),
    ("css", &CSS),
    ("csx", &CSHARP),
    ("cts", &TYPESCRIPT),
    ("cu", &SHADER),
    ("cuda", &SHADER),
    ("cuh", &SHADER),
    ("cxx", &CPP),
    ("dart", &DART),
    ("ddl", &SQL),
    ("desktop", &INI),
    ("diff", &DIFF),
    ("dml", &SQL),
    ("docker", &DOCKERFILE),
    ("dockerfile", &DOCKERFILE),
    ("dos", &BATCH),
    ("dotenv", &SHELL),
    ("editorconfig", &INI),
    ("edn", &LISP),
    ("ejs", &XML),
    ("el", &LISP),
    ("elisp", &LISP),
    ("elixir", &ELIXIR),
    ("elm", &ELM),
    ("emacs", &LISP),
    ("env", &SHELL),
    ("erb", &XML),
    ("erl", &ERLANG),
    ("erlang", &ERLANG),
    ("ex", &ELIXIR),
    ("exs", &ELIXIR),
    ("f", &FORTRAN),
    ("f#", &OCAML),
    ("f03", &FORTRAN),
    ("f08", &FORTRAN),
    ("f77", &FORTRAN),
    ("f90", &FORTRAN),
    ("f95", &FORTRAN),
    ("fastfile", &RUBY),
    ("fish", &SHELL),
    ("fnl", &LISP),
    ("for", &FORTRAN),
    ("fortran", &FORTRAN),
    ("frag", &SHADER),
    ("fs", &OCAML),
    ("fsharp", &OCAML),
    ("fsi", &OCAML),
    ("fsproj", &XML),
    ("fsx", &OCAML),
    ("fx", &SHADER),
    ("gemfile", &RUBY),
    ("gemspec", &RUBY),
    ("geojson", &JSON),
    ("geom", &SHADER),
    ("gitconfig", &INI),
    ("gleam", &GLEAM),
    ("glsl", &SHADER),
    ("gnumakefile", &MAKEFILE),
    ("go", &GO),
    ("go.mod", &GO),
    ("golang", &GO),
    ("gpx", &XML),
    ("gql", &GRAPHQL),
    ("gradle", &GROOVY),
    ("graphql", &GRAPHQL),
    ("groovy", &GROOVY),
    ("guardfile", &RUBY),
    ("gvy", &GROOVY),
    ("gyp", &PYTHON),
    ("h", &C),
    ("h++", &CPP),
    ("handlebars", &XML),
    ("har", &JSON),
    ("haskell", &HASKELL),
    ("hbs", &XML),
    ("hcl", &HCL),
    ("heex", &ELIXIR),
    ("hh", &CPP),
    ("hlsl", &SHADER),
    ("hpp", &CPP),
    ("hrl", &ERLANG),
    ("hs", &HASKELL),
    ("htm", &XML),
    ("html", &XML),
    ("hxx", &CPP),
    ("ini", &INI),
    ("inl", &CPP),
    ("ino", &CPP),
    ("ipp", &CPP),
    ("ipynb", &JSON),
    ("j2", &XML),
    ("java", &JAVA),
    ("javascript", &JAVASCRIPT),
    ("jenkinsfile", &GROOVY),
    ("jinja", &XML),
    ("jinja2", &XML),
    ("jl", &JULIA),
    ("js", &JAVASCRIPT),
    ("json", &JSON),
    ("json5", &JSON),
    ("jsonc", &JSON),
    ("jsonl", &JSON),
    ("jsp", &XML),
    ("jsx", &JSX),
    ("julia", &JULIA),
    ("justfile", &MAKEFILE),
    ("kml", &XML),
    ("kotlin", &KOTLIN),
    ("ksh", &SHELL),
    ("kt", &KOTLIN),
    ("kts", &KOTLIN),
    ("latex", &LATEX),
    ("less", &SCSS),
    ("lhs", &HASKELL),
    ("liquid", &XML),
    ("lisp", &LISP),
    ("log", &LOG),
    ("lsp", &LISP),
    ("ltx", &LATEX),
    ("lua", &LUA),
    ("luau", &LUA),
    ("m", &OBJECTIVEC),
    ("mak", &MAKEFILE),
    ("make", &MAKEFILE),
    ("makefile", &MAKEFILE),
    ("markdown", &MARKDOWN),
    ("md", &MARKDOWN),
    ("mdown", &MARKDOWN),
    ("mdx", &MARKDOWN),
    ("metal", &SHADER),
    ("mjs", &JAVASCRIPT),
    ("mk", &MAKEFILE),
    ("mkd", &MARKDOWN),
    ("mkdn", &MARKDOWN),
    ("ml", &OCAML),
    ("mli", &OCAML),
    ("mm", &OBJECTIVEC),
    ("mojo", &PYTHON),
    ("mts", &TYPESCRIPT),
    ("mustache", &XML),
    ("mysql", &SQL),
    ("nasm", &ASSEMBLY),
    ("ndjson", &JSON),
    ("nim", &NIM),
    ("nimble", &NIM),
    ("nims", &NIM),
    ("nix", &NIX),
    ("njk", &XML),
    ("node", &JAVASCRIPT),
    ("nomad", &HCL),
    ("objc", &OBJECTIVEC),
    ("objective-c", &OBJECTIVEC),
    ("objectivec", &OBJECTIVEC),
    ("ocaml", &OCAML),
    ("patch", &DIFF),
    ("pcss", &SCSS),
    ("perl", &PERL),
    ("pgsql", &SQL),
    ("php", &PHP),
    ("phtml", &PHP),
    ("pipfile", &TOML),
    ("pl", &PERL),
    ("plist", &XML),
    ("pm", &PERL),
    ("pod", &PERL),
    ("podfile", &RUBY),
    ("podspec", &RUBY),
    ("poetry.lock", &TOML),
    ("powershell", &POWERSHELL),
    ("prefs", &INI),
    ("prisma", &GRAPHQL),
    ("procfile", &YAML),
    ("properties", &INI),
    ("props", &XML),
    ("proto", &GRAPHQL),
    ("protobuf", &GRAPHQL),
    ("ps1", &POWERSHELL),
    ("psd1", &POWERSHELL),
    ("psm1", &POWERSHELL),
    ("psql", &SQL),
    ("purs", &HASKELL),
    ("pwsh", &POWERSHELL),
    ("pxd", &PYTHON),
    ("py", &PYTHON),
    ("pyi", &PYTHON),
    ("python", &PYTHON),
    ("python3", &PYTHON),
    ("pyw", &PYTHON),
    ("pyx", &PYTHON),
    ("qmd", &MARKDOWN),
    ("r", &R),
    ("racket", &LISP),
    ("rake", &RUBY),
    ("rakefile", &RUBY),
    ("razor", &XML),
    ("rb", &RUBY),
    ("re", &OCAML),
    ("reg", &INI),
    ("rej", &DIFF),
    ("resx", &XML),
    ("rkt", &LISP),
    ("rmd", &MARKDOWN),
    ("rs", &RUST),
    ("rss", &XML),
    ("ru", &RUBY),
    ("ruby", &RUBY),
    ("rust", &RUST),
    ("s", &ASSEMBLY),
    ("sass", &SCSS),
    ("sbt", &SCALA),
    ("sc", &SCALA),
    ("scala", &SCALA),
    ("scheme", &LISP),
    ("scm", &LISP),
    ("scss", &SCSS),
    ("service", &INI),
    ("sh", &SHELL),
    ("shader", &SHADER),
    ("shell", &SHELL),
    ("shellsession", &SHELL),
    ("sol", &SOLIDITY),
    ("solidity", &SOLIDITY),
    ("sql", &SQL),
    ("ss", &LISP),
    ("star", &PYTHON),
    ("storyboard", &XML),
    ("sty", &LATEX),
    ("styl", &SCSS),
    ("sv", &VERILOG),
    ("svelte", &XML),
    ("svg", &XML),
    ("svh", &VERILOG),
    ("swift", &SWIFT),
    ("systemverilog", &VERILOG),
    ("targets", &XML),
    ("tcl", &TCL),
    ("terminal", &SHELL),
    ("terraform", &HCL),
    ("tesc", &SHADER),
    ("tese", &SHADER),
    ("tex", &LATEX),
    ("tf", &HCL),
    ("tfvars", &HCL),
    ("thrift", &GRAPHQL),
    ("toml", &TOML),
    ("topojson", &JSON),
    ("tpp", &CPP),
    ("ts", &TYPESCRIPT),
    ("tsx", &TSX),
    ("twig", &XML),
    ("typescript", &TYPESCRIPT),
    ("udiff", &DIFF),
    ("v", &VERILOG),
    ("vagrantfile", &RUBY),
    ("vb", &VB),
    ("vba", &VB),
    ("vbnet", &VB),
    ("vbproj", &XML),
    ("vbs", &VB),
    ("verilog", &VERILOG),
    ("vert", &SHADER),
    ("vh", &VERILOG),
    ("vhd", &VHDL),
    ("vhdl", &VHDL),
    ("vue", &XML),
    ("webmanifest", &JSON),
    ("wgsl", &SHADER),
    ("wsdl", &XML),
    ("xaml", &XML),
    ("xhtml", &XML),
    ("xib", &XML),
    ("xml", &XML),
    ("xsd", &XML),
    ("xsl", &XML),
    ("xslt", &XML),
    ("yaml", &YAML),
    ("yarn.lock", &YAML),
    ("yml", &YAML),
    ("zig", &ZIG),
    ("zon", &ZIG),
    ("zsh", &SHELL),
];
