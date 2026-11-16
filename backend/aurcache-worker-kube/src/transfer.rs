//! The exchanges with a build pod, each one `exec` of the agent: handing the
//! job in (`receive`), following the log (`tail`), and handing the packages
//! back (`manifest`, then one `send` per package relayed straight into its
//! upload).
//!
//! Nothing here holds a package: no temporary file, no buffer of a whole file.

// WP9 writes this module.
