use crate::kernel::io::{FileExtension, IoFactory};
use crate::kernel::lsm::compactor::LEVEL_0;
use crate::kernel::lsm::storage::{Config, Gen};
use crate::kernel::lsm::table::loader::TableLoader;
use crate::kernel::lsm::table::meta::TableMeta;
use crate::kernel::lsm::table::scope::Scope;
use crate::kernel::lsm::table::Table;
use crate::kernel::lsm::version::cleaner::CleanTag;
use crate::kernel::lsm::version::edit::{EditType, VersionEdit};
use crate::kernel::lsm::version::meta::VersionMeta;
use crate::kernel::{sorted_gen_list, Result};
use bytes::Bytes;
use itertools::Itertools;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info};

mod cleaner;
pub(crate) mod edit;
pub(crate) mod iter;
mod meta;
pub(crate) mod status;
#[cfg(test)]
mod test;

pub(crate) const DEFAULT_SS_TABLE_PATH: &str = "ss_table";
pub(crate) const DEFAULT_VERSION_PATH: &str = "version";
pub(crate) const DEFAULT_VERSION_LOG_THRESHOLD: usize = 233;

pub(crate) type LevelSlice = [Vec<Scope>; 7];

fn snapshot_gen(factory: &IoFactory) -> Result<i64> {
    if let Ok(gen_list) = sorted_gen_list(factory.get_path(), FileExtension::Log) {
        return Ok(match *gen_list.as_slice() {
            [.., old_snapshot, new_snapshot] => {
                factory.clean(new_snapshot)?;
                old_snapshot
            }
            [snapshot] => snapshot,
            _ => Gen::create(),
        });
    }

    Ok(Gen::create())
}

#[derive(Clone)]
pub(crate) struct Version {
    pub(crate) version_num: u64,
    /// Table存储Map
    /// 全局共享
    table_loader: Arc<TableLoader>,
    /// Level层级Vec
    /// 以索引0为level-0这样的递推，存储文件的gen值
    /// 每个Version各持有各自的Gen矩阵
    pub(crate) level_slice: LevelSlice,
    /// 统计数据
    pub(crate) meta_data: VersionMeta,
    /// 清除信号发送器
    /// Drop时通知Cleaner进行删除
    clean_tx: UnboundedSender<CleanTag>,
}

impl Version {
    pub(crate) fn len(&self) -> usize {
        self.meta_data.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn level_len(&self, level: usize) -> usize {
        self.level_slice[level].len()
    }

    pub(crate) fn size_of_disk(&self) -> u64 {
        self.meta_data.size_of_disk
    }

    /// 通过一组VersionEdit载入Version
    pub(crate) fn load_from_log(
        vec_log: Vec<VersionEdit>,
        ss_table_loader: &Arc<TableLoader>,
        clean_tx: UnboundedSender<CleanTag>,
    ) -> Result<Self> {
        let mut version = Self {
            version_num: 0,
            table_loader: Arc::clone(ss_table_loader),
            level_slice: Self::level_slice_new(),
            meta_data: VersionMeta {
                size_of_disk: 0,
                len: 0,
            },
            clean_tx,
        };

        version.apply(vec_log)?;
        version_display(&version, "load_from_log");

        Ok(version)
    }

    /// Version对VersionEdit的应用处理
    ///
    /// Tips: 当此处像Cleaner发送Tag::Add时，此时的version中不需要的gens
    /// 因此每次删除都是删除此Version的前一位所需要删除的Version
    /// 也就是可能存在一次Version的冗余Table
    /// 可能是个确定，但是Minor Compactor比较起来更加频繁，也就是大多数情况不会冗余，因此我觉得影响较小
    /// 也可以算作是一种Major Compaction异常时的备份？
    pub(crate) fn apply(&mut self, vec_version_edit: Vec<VersionEdit>) -> Result<()> {
        let mut del_gens = Vec::new();
        let mut vec_statistics_sst_meta = Vec::new();

        for version_edit in vec_version_edit {
            match version_edit {
                VersionEdit::DeleteFile((mut vec_gen, level), sst_meta) => {
                    vec_statistics_sst_meta.push(EditType::Del(sst_meta));

                    self.level_slice[level].retain(|scope| !vec_gen.contains(&scope.get_gen()));
                    del_gens.append(&mut vec_gen);
                }
                VersionEdit::NewFile((vec_scope, level), index, sst_meta) => {
                    vec_statistics_sst_meta.push(EditType::Add(sst_meta));

                    // Level 0中的Table绝对是以gen为优先级
                    // Level N中则不以gen为顺序，此处对gen排序是因为单次NewFile中的gen肯定是有序的
                    let scope_iter = vec_scope.into_iter().sorted_by_key(Scope::get_gen);
                    if level == LEVEL_0 {
                        for scope in scope_iter {
                            self.level_slice[level].push(scope);
                        }
                    } else {
                        for scope in scope_iter.rev() {
                            self.level_slice[level].insert(index, scope);
                        }
                    }
                }
            }
        }

        self.meta_data
            .statistical_process(vec_statistics_sst_meta)?;
        self.version_num += 1;
        self.clean_tx.send(CleanTag::Add {
            version: self.version_num,
            gens: del_gens,
        })?;

        Ok(())
    }

    fn level_slice_new() -> [Vec<Scope>; 7] {
        [
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ]
    }

    /// 把当前version的leveSlice中的数据转化为一组versionEdit 作为新version_log的base
    pub(crate) fn to_vec_edit(&self) -> Vec<VersionEdit> {
        fn sst_meta_by_level(level: usize, size_of_disk: u64, len: usize) -> TableMeta {
            if LEVEL_0 == level {
                TableMeta { size_of_disk, len }
            } else {
                TableMeta::default()
            }
        }

        self.level_slice
            .iter()
            .enumerate()
            .filter_map(|(level, vec_scope)| {
                (!vec_scope.is_empty()).then(|| {
                    VersionEdit::NewFile(
                        (vec_scope.clone(), level),
                        0,
                        sst_meta_by_level(level, self.size_of_disk(), self.len()),
                    )
                })
            })
            .collect_vec()
    }

    pub(crate) fn table(&self, level: usize, offset: usize) -> Option<&dyn Table> {
        self.level_slice[level]
            .get(offset)
            .and_then(|scope| self.table_loader.get(scope.get_gen()))
    }

    /// 只限定获取Level 0的Table
    ///
    /// Tips：不鼓励全量获取其他Level
    pub(crate) fn tables_by_level_0(&self) -> Vec<&dyn Table> {
        self.level_slice[LEVEL_0]
            .iter()
            .filter_map(|scope| self.table_loader.get(scope.get_gen()))
            .collect_vec()
    }

    pub(crate) fn index(&self, level: usize, source_gen: i64) -> Option<usize> {
        self.level_slice[level]
            .iter()
            .enumerate()
            .find(|(_, scope)| source_gen.eq(&scope.get_gen()))
            .map(|(index, _)| index)
    }

    pub(crate) fn first_tables(
        &self,
        level: usize,
        size: usize,
    ) -> Option<(Vec<&dyn Table>, Vec<Scope>)> {
        if self.level_slice[level].is_empty() {
            return None;
        }

        Some(
            self.level_slice[level]
                .iter()
                .take(size)
                .filter_map(|scope| {
                    self.table_loader
                        .get(scope.get_gen())
                        .map(|ss_table| (ss_table, scope.clone()))
                })
                .unzip(),
        )
    }

    /// 获取指定level中与scope冲突的Tables和Scopes
    pub(crate) fn tables_by_scopes(
        &self,
        level: usize,
        target_scope: &Scope,
    ) -> (Vec<&dyn Table>, Vec<Scope>) {
        self.level_slice[level]
            .iter()
            .filter(|scope| scope.meet(target_scope))
            .filter_map(|scope| {
                self.table_loader
                    .get(scope.get_gen())
                    .map(|ss_table| (ss_table, scope.clone()))
            })
            .unzip()
    }

    /// 获取指定level中与scope冲突的Tables
    pub(crate) fn tables_by_meet_scope<F>(&self, level: usize, fn_meet: F) -> Vec<&dyn Table>
    where
        F: Fn(&Scope) -> bool,
    {
        self.level_slice[level]
            .iter()
            .filter(|scope| fn_meet(scope))
            .filter_map(|scope| self.table_loader.get(scope.get_gen()))
            .collect_vec()
    }

    /// 使用Key从现有Tables中获取对应的数据
    pub(crate) fn query(&self, key: &[u8]) -> Result<Option<Bytes>> {
        let table_loader = &self.table_loader;
        // Level 0的Table是无序且Table间的数据是可能重复的,因此需要遍历
        for scope in self.level_slice[LEVEL_0].iter().rev() {
            if scope.meet_by_key(key) {
                if let Some(ss_table) = table_loader.get(scope.get_gen()) {
                    if let Some(value) = ss_table.query(key)? {
                        return Ok(Some(value));
                    }
                }
            }
        }
        // Level 1-7的数据排布有序且唯一，因此在每一个等级可以直接找到唯一一个Key可能在范围内的Table
        for level in 1..7 {
            let offset = self.query_meet_index(key, level);

            if let Some(scope) = self.level_slice[level].get(offset) {
                return if let Some(ss_table) = table_loader.get(scope.get_gen()) {
                    ss_table.query(key)
                } else {
                    Ok(None)
                };
            }
        }

        Ok(None)
    }

    /// 获取指定Table索引位置
    pub(crate) fn find_index_by_level(&self, option_first: Option<i64>, level: usize) -> usize {
        option_first
            .and_then(|gen| self.index(level, gen))
            .unwrap_or(0)
    }

    pub(crate) fn query_meet_index(&self, key: &[u8], level: usize) -> usize {
        self.level_slice[level]
            .binary_search_by(|scope| scope.start.as_ref().cmp(key))
            .unwrap_or_else(|index| index.saturating_sub(1))
    }

    /// 判断是否溢出指定的Table数量
    pub(crate) fn is_threshold_exceeded_major(&self, config: &Config, level: usize) -> bool {
        self.level_slice[level].len()
            >= (config.major_threshold_with_sst_size
                * config.level_sst_magnification.pow(level as u32))
    }
}

impl Drop for Version {
    /// 将此Version可删除的版本号发送
    fn drop(&mut self) {
        if self
            .clean_tx
            .send(CleanTag::Clean(self.version_num))
            .is_err()
        {
            error!(
                "[Cleaner][clean][Version: {}]: Channel Close!",
                self.version_num
            );
        }
    }
}

/// 使用特定格式进行display
pub(crate) fn version_display(new_version: &Version, method: &str) {
    info!(
        "[Version: {}]: version_num: {}, len: {}, size_of_disk: {}",
        method,
        new_version.version_num,
        new_version.len(),
        new_version.size_of_disk(),
    );
}
