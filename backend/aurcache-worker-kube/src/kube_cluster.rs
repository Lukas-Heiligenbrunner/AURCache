//! [`Cluster`] over a real API server, through kube-rs.
//!
//! One reflector on the builds' namespace, filtered by this dispatcher's
//! label, feeds [`Cluster::pod`], [`Cluster::pods`] and
//! [`Cluster::generation`]; everything else is one API call. An exec's stdout
//! is read only as fast as the caller reads it, so nothing is buffered whole.

use crate::cluster::{Admission, Cluster, CreateError, Exec, Stdin};
use anyhow::Result;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::reflector::Store;
use std::sync::Arc;
use std::time::Duration;

// WP5 writes everything below; until then nothing constructs or reads it.
#[allow(dead_code)]
pub struct KubeCluster {
    client: kube::Client,
    namespace: String,
    store: Store<Pod>,
    generation: tokio::sync::watch::Receiver<u64>,
}

impl KubeCluster {
    /// Connect (in-cluster, or `KUBECONFIG`), start the watch on this
    /// dispatcher's pods, and return once it has listed them.
    pub async fn connect(namespace: &str, dispatcher: &str) -> Result<Self> {
        let _ = (namespace, dispatcher);
        unimplemented!()
    }

    /// The image a pod runs, by digest where its status has a pullable one.
    pub async fn image_of(&self, namespace: &str, pod: &str) -> Result<String> {
        let _ = (namespace, pod);
        unimplemented!()
    }
}

impl Cluster for KubeCluster {
    fn create(&self, pod: &Pod) -> impl Future<Output = Result<(), CreateError>> + Send {
        let _ = pod;
        async { unimplemented!() }
    }

    fn admits(&self, pod: &Pod) -> impl Future<Output = Admission> + Send {
        let _ = pod;
        async { unimplemented!() }
    }

    fn delete(&self, name: &str, grace: Duration) -> impl Future<Output = Result<()>> + Send {
        let _ = (name, grace);
        async { unimplemented!() }
    }

    async fn sweep(&self) -> Result<usize> {
        unimplemented!()
    }

    fn pod(&self, name: &str) -> Option<Arc<Pod>> {
        let _ = name;
        unimplemented!()
    }

    fn pods(&self) -> Vec<Arc<Pod>> {
        unimplemented!()
    }

    fn generation(&self) -> tokio::sync::watch::Receiver<u64> {
        unimplemented!()
    }

    fn exec(
        &self,
        pod: &str,
        container: &str,
        argv: &[&str],
        stdin: Stdin,
    ) -> impl Future<Output = Result<Exec>> + Send {
        let _ = (pod, container, argv, stdin);
        async { unimplemented!() }
    }

    fn log_tail(
        &self,
        pod: &str,
        container: &str,
        lines: i64,
    ) -> impl Future<Output = Result<String>> + Send {
        let _ = (pod, container, lines);
        async { unimplemented!() }
    }
}
