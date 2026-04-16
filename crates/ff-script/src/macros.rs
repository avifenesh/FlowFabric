//! The `ff_function!` declarative macro for typed FCALL wrappers.
//!
//! Generates an async function that:
//! 1. Builds KEYS and ARGV vectors from the provided expressions
//! 2. Calls `conn.fcall::<Value>(fn_name, &keys, &argv)`
//! 3. Parses the result via `FromFcallResult`
//!
//! # Example
//!
//! ```ignore
//! ff_function! {
//!     /// Complete an active execution.
//!     pub ff_complete_execution(args: CompleteExecutionArgs) -> CompleteExecutionResult {
//!         keys(ctx: &ExecKeyContext) {
//!             ctx.core(),
//!             ctx.attempt_hash(args.attempt_index),
//!             ctx.lease_current(),
//!         }
//!         argv {
//!             args.execution_id.to_string(),
//!             args.lease_id.to_string(),
//!         }
//!     }
//! }
//! ```

/// Generate typed async FCALL wrappers from a declarative specification.
///
/// Each invocation defines one or more functions. The macro generates:
/// - An `async fn` that takes `conn`, a key context, and typed args
/// - Builds KEYS and ARGV vectors from the provided expressions
/// - Calls `FCALL <function_name>` via the ferriskey client
/// - Parses the result via the `FromFcallResult` trait
#[macro_export]
macro_rules! ff_function {
    // Support multiple function definitions in one block
    (
        $(
            $(#[$attr:meta])*
            $vis:vis $fn_name:ident (
                $args_name:ident : $args_type:ty
            ) -> $result_type:ty {
                keys( $ctx_name:ident : & $ctx_type:ty ) {
                    $( $key_expr:expr ),* $(,)?
                }
                argv {
                    $( $argv_expr:expr ),* $(,)?
                }
            }
        )*
    ) => {
        $(
            $(#[$attr])*
            $vis async fn $fn_name(
                conn: &ferriskey::Client,
                $ctx_name: &$ctx_type,
                $args_name: &$args_type,
            ) -> ::core::result::Result<$result_type, $crate::error::ScriptError> {
                let keys: ::std::vec::Vec<String> = vec![ $( $key_expr ),* ];
                let argv: ::std::vec::Vec<String> = vec![ $( $argv_expr ),* ];
                let key_refs: ::std::vec::Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
                let argv_refs: ::std::vec::Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
                let raw = conn
                    .fcall::<ferriskey::Value>(stringify!($fn_name), &key_refs, &argv_refs)
                    .await
                    .map_err($crate::error::ScriptError::Valkey)?;
                <$result_type as $crate::result::FromFcallResult>::from_fcall_result(&raw)
            }
        )*
    };
}
