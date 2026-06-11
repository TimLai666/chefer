//! depends_on 拓撲排序（Kahn 演算法），用於決定服務啟動順序。

use std::collections::{BTreeMap, VecDeque};

use anyhow::{Result, bail};

use crate::manifest::ServiceEntry;

/// 回傳依啟動順序排列的服務參考；偵測缺失依賴與循環。
/// 同一層級（無相依先後）以 name 排序，輸出具確定性。
pub fn topo_sort(services: &[ServiceEntry]) -> Result<Vec<&ServiceEntry>> {
    let by_name: BTreeMap<&str, &ServiceEntry> =
        services.iter().map(|s| (s.name.as_str(), s)).collect();

    // in-degree = 此服務尚未滿足的依賴數
    let mut indeg: BTreeMap<&str, usize> = BTreeMap::new();
    let mut rdeps: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for s in services {
        indeg.entry(s.name.as_str()).or_insert(0);
        for d in &s.depends_on {
            if !by_name.contains_key(d.as_str()) {
                bail!("service `{}` 依賴不存在的 service `{}`", s.name, d);
            }
            if d == &s.name {
                bail!("service `{}` 不可依賴自己", s.name);
            }
            *indeg.entry(s.name.as_str()).or_insert(0) += 1;
            rdeps.entry(d.as_str()).or_default().push(s.name.as_str());
        }
    }

    // BTreeMap 迭代有序 → 起始佇列依 name 排序
    let mut queue: VecDeque<&str> = indeg
        .iter()
        .filter(|&(_, &d)| d == 0)
        .map(|(&n, _)| n)
        .collect();

    let mut order = Vec::with_capacity(services.len());
    while let Some(n) = queue.pop_front() {
        order.push(by_name[n]);
        if let Some(deps) = rdeps.get(n) {
            // 收集本輪解鎖者，排序後入列以維持確定性
            let mut unlocked: Vec<&str> = Vec::new();
            for &m in deps {
                let d = indeg.get_mut(m).unwrap();
                *d -= 1;
                if *d == 0 {
                    unlocked.push(m);
                }
            }
            unlocked.sort_unstable();
            queue.extend(unlocked);
        }
    }

    if order.len() != services.len() {
        let stuck: Vec<&str> = indeg
            .iter()
            .filter(|&(_, &d)| d > 0)
            .map(|(&n, _)| n)
            .collect();
        bail!("depends_on 存在循環依賴，無法決定啟動順序：{}", stuck.join(", "));
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ImageConfig, InterfaceMode, ServiceEntry};
    use std::collections::BTreeMap;

    fn svc(name: &str, deps: &[&str]) -> ServiceEntry {
        ServiceEntry {
            name: name.into(),
            platform: "linux/amd64".into(),
            layers: vec![],
            image_config: ImageConfig::default(),
            cmd_override: None,
            env: BTreeMap::new(),
            workdir_override: None,
            persist_path: None,
            ports: vec![],
            mounts: vec![],
            interface_mode: InterfaceMode::None,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn orders_dependencies_first() {
        let services = vec![svc("ui", &["db"]), svc("db", &[]), svc("worker", &["db", "ui"])];
        let order: Vec<&str> = topo_sort(&services)
            .unwrap()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(order, vec!["db", "ui", "worker"]);
    }

    #[test]
    fn detects_cycle() {
        let services = vec![svc("a", &["b"]), svc("b", &["a"])];
        assert!(topo_sort(&services).is_err());
    }

    #[test]
    fn detects_missing_dep() {
        let services = vec![svc("a", &["nope"])];
        assert!(topo_sort(&services).is_err());
    }

    #[test]
    fn detects_self_dep() {
        let services = vec![svc("a", &["a"])];
        assert!(topo_sort(&services).is_err());
    }
}
