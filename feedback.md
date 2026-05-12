 Best parts:
 - Exact local version/features. Big win over docs.rs.
 - Output compact. Good for agent context.
 - Re-exports mostly work. Important.
 - Traits show associated items. Very useful.
 - Fields/variants shown. Useful.
 - Errors mostly actionable.

 Current pain points:
 1. Missing-id re-export bug hurts trust
# FIXED; 
     - serde::Serialize worked in axum but failed in this project.
     - futures_util::future::BoxFuture failed same way.
     - Agent sees “not found” for real item. Bad.
     - Fix this first. Error should say external re-export or unsupported rustdoc external id, not “missing from index”.
 2. Module query behavior is confusing
# FIXED; 
     - tokio::sync::watch returns item: item watch, definition: pub module watch.
     - That is okay, but skill says query concrete item.
     - Better: support modules explicitly and label item: module watch.
     - Or error with “module found; query concrete item like Sender/Receiver/channel”.
 3. Bad module-name hint 
# FIXED; 
     - __define_rejection appears to be module because lowercase heuristic. “Sender” example nonsense.
     - Remove generic Sender hint or make it neutral:
           - “query a concrete item inside this module”
     - For macro-ish names, do not module-hint.
 4. Function signatures too lossy
# FIXED; 
     - Example:
           - poll_ready(self: &mut Self, cx: &mut Context) -> Poll
           - Missing generic args like Poll<Result<(), Self::Error>>.
     - from_request_parts -> impl Trait too vague.
     - For coding, return type precision matters. Improve type formatter for ResolvedPath args, associated types, impl trait bounds, Result
 args.
 5. Structs with private fields show { }
     - Fine, but say “fields private/stripped” if rustdoc knows.
     - Current output can look like empty struct.
 6. Derives absent
     - Skill says derives when available, but Rustdoc JSON attrs probably won’t preserve derive reliably. Either implement or stop promising.
 7. Methods unsupported
     - Acceptable for v1/v2, but big usability gap. Agents often need “what methods on Bytes?”
     - Future feature: check-docs 'impl bytes::Bytes' or check-docs 'use bytes::Bytes;' --methods.
 8. Multi-item brace unsupported
     - Not huge. Easy later. Agents can split.

 What I’d change first:
 1. Fix missing-id re-export handling.
 2. Improve type formatting for generics/associated types.
 3. Label modules properly.
 4. Remove bad Sender hint.
 5. Add note: lines for stripped/private fields and feature-gated not-found suspicion.

 Desired output shape:

 ```text
   crate: futures-util 0.3.31
   import: use futures_util::future::BoxFuture;
   item: type BoxFuture
   location: ...
   definition: pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
   docs: ...
 ```

 For failure:

 ```text
   check-docs: item 'Serialize' is a public re-export from external crate 'serde_core'; full item data is not present in serde rustdoc JSON.
 Add/query direct dependency `serde_core` or enable external re-export resolution.
 ```

 Verdict:
 - Usable now: yes.
 - Agent-useful: yes, very.
 - Trustworthy enough: mostly, but missing-id bug can mislead.
 - Biggest value: exact dependency/version/features in compact CLI form.
 - Next milestone: make failures honest and signatures more precise.

