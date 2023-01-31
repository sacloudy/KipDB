use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use async_trait::async_trait;
use fslock::LockFile;
use itertools::Itertools;
use snowflake::SnowflakeIdGenerator;
use tokio::sync::{Mutex, oneshot};
use tokio::sync::oneshot::Sender;
use tracing::{error, info};
use crate::KvsError;
use crate::kernel::{CommandData, DEFAULT_LOCK_FILE, KVStore, lock_or_time_out};
use crate::kernel::io::FileExtension;
use crate::kernel::lsm::{MemMap, MemTable};
use crate::kernel::lsm::compactor::Compactor;
use crate::kernel::lsm::log::LogLoader;
use crate::kernel::lsm::mvcc::Transaction;
use crate::kernel::lsm::version::VersionStatus;
use crate::kernel::Result;

pub(crate) const DEFAULT_MINOR_THRESHOLD_WITH_LEN: usize = 2333;

pub(crate) const DEFAULT_SPARSE_INDEX_INTERVAL_BLOCK_SIZE: u64 = 4;

pub(crate) const DEFAULT_SST_FILE_SIZE: usize = 24 * 1024 * 1024;

pub(crate) const DEFAULT_MAJOR_THRESHOLD_WITH_SST_SIZE: usize = 10;

pub(crate) const DEFAULT_MAJOR_SELECT_FILE_SIZE: usize = 3;

pub(crate) const DEFAULT_LEVEL_SST_MAGNIFICATION: usize = 10;

pub(crate) const DEFAULT_DESIRED_ERROR_PROB: f64 = 0.05;

pub(crate) const DEFAULT_BLOCK_CACHE_SIZE: usize = 3200;

pub(crate) const DEFAULT_TABLE_CACHE_SIZE: usize = 112;

pub(crate) const DEFAULT_WAL_THRESHOLD: usize = 20;

pub(crate) const DEFAULT_WAL_PATH: &str = "wal";

/// 基于LSM的KV Store存储内核
/// Leveled Compaction压缩算法
pub struct LsmStore {
    /// MemTable
    /// https://zhuanlan.zhihu.com/p/79064869
    pub(crate) mem_table: MemTable,
    /// VersionVec
    /// 用于管理内部多版本状态
    pub(crate) ver_status: Arc<VersionStatus>,
    /// LSM全局参数配置
    config: Arc<Config>,
    /// WAL载入器
    ///
    /// 用于异常停机时MemTable的恢复
    /// 同时当Level 0的SSTable异常时，可以尝试恢复
    /// `Config.wal_threshold`用于控制WalLoader的的SSTable数据日志个数
    /// 超出个数阈值时会清空最旧的一半日志
    wal: Arc<LogLoader>,
    /// 多进程文件锁
    /// 避免多进程进行数据读写
    lock_file: LockFile,
    /// 异步任务阻塞监听器
    vec_rev: Mutex<Vec<oneshot::Receiver<()>>>,
    /// 单线程压缩器
    compactor: Arc<Mutex<Compactor>>
}

#[async_trait]
impl KVStore for LsmStore {
    #[inline]
    fn name() -> &'static str where Self: Sized {
        "LSMStore made in Kould"
    }

    #[inline]
    async fn open(path: impl Into<PathBuf> + Send) -> Result<Self> {
        LsmStore::open_with_config(Config::new(path, 0, 0)).await
    }

    #[inline]
    async fn flush(&self) -> Result<()> {
        self.flush_(false).await
    }

    #[inline]
    async fn set(&self, key: &[u8], value: Vec<u8>) -> Result<()> {
        self.append_cmd_data(
            CommandData::set(key.to_vec(), value), true
        ).await
    }

    #[inline]
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(value) = self.mem_table.find(key) {
            return Ok(Some(value));
        }

        // 读取前等待压缩完毕
        // 相对来说，消耗较小
        // 当压缩时长高时，说明数据量非常大
        // 此时直接去获取的话可能会既获取不到数据，也花费大量时间
        self.wait_for_compression_down().await?;

        if let Some(value) = self.ver_status
            .current().await
            .find_data_for_ss_tables(key).await?
        {
            return Ok(Some(value));
        }

        Ok(None)
    }

    #[inline]
    async fn remove(&self, key: &[u8]) -> Result<()> {
        match self.get(key).await? {
            Some(_) => {
                self.append_cmd_data(
                   CommandData::remove(key.to_vec()), true
                ).await
            }
            None => { Err(KvsError::KeyNotFound) }
        }
    }

    #[inline]
    async fn size_of_disk(&self) -> Result<u64> {
        Ok(self.ver_status.current().await
            .get_size_of_disk())
    }

    #[inline]
    async fn len(&self) -> Result<usize> {
        Ok(self.ver_status.current().await
            .get_len()
            + self.mem_table.len())
    }

    #[inline]
    async fn is_empty(&self) -> bool {
        self.ver_status.current().await
            .is_empty()
            && self.mem_table.is_empty()
    }
}

impl Drop for LsmStore {
    fn drop(&mut self) {
        // 自旋获取Compactor的锁进行Drop
        // tip: tokio的Mutex的lock方法是async的
        loop {
            if let Some(_compactor) = self.compactor.try_lock().ok() {
                self.lock_file.unlock()
                    .expect("LockFile unlock failed!");
                break
            }
        }
    }
}

impl LsmStore {

    /// 追加数据
    async fn append_cmd_data(&self, cmd: CommandData, wal_write: bool) -> Result<()> {
        let mem_table = &self.mem_table;

        // Wal与MemTable双写
        if self.is_enable_wal() && wal_write {
            wal_put(
                &self.wal, &cmd, !self.is_async_wal()
            ).await;
        }

        if mem_table.insert_data_and_is_exceeded(cmd, &self.config) {
            self.minor_compaction().await?;
        }

        Ok(())
    }

    fn is_enable_wal(&self) -> bool {
        self.config.wal_enable
    }

    fn is_async_wal(&self) -> bool {
        self.config.wal_async_put_enable
    }

    /// 存活标记
    /// 返回一个Sender用于存活结束通知
    async fn live_tag(&self) -> Sender<()> {
        let (sender, receiver) = oneshot::channel();

        self.vec_rev.lock()
            .await
            .push(receiver);

        sender
    }

    /// 等待所有压缩结束
    pub(crate) async fn wait_for_compression_down(&self) -> Result<()> {
        // 监听异步任务是否执行完毕
        let mut vec_rev = self.vec_rev.lock().await;
        while let Some(rev) = vec_rev.pop() {
            rev.await?
        }

        Ok(())
    }

    /// 使用Config进行LsmStore初始化
    #[inline]
    pub async fn open_with_config(config: Config) -> Result<Self> where Self: Sized {
        let config = Arc::new(config);
        // 若lockfile的文件夹路径不存在则创建
        fs::create_dir_all(&config.dir_path)?;
        let lock_file = lock_or_time_out(
            &config.dir_path.join(DEFAULT_LOCK_FILE)
        ).await?;

        let (wal, option_success) = LogLoader::reload_with_check(
            &config,
            DEFAULT_WAL_PATH,
            FileExtension::Log
        )?;

        let mem_map = match option_success {
            None => MemMap::new(),
            Some(vec_data) => {
                // Q: 为什么此处直接生成新的seq并使限制每个数据都使用?
                // A: 因为此处是当存在有停机异常时使用wal恢复数据,此处也不存在有Version(VersionStatus的初始化在此代码之后)
                // 因此不会影响Version的读取顺序
                let create_seq_id = config.create_gen_lazy();
                MemMap::from_iter(
                    // 倒序唯一化，保留最新的数据
                    vec_data.into_iter()
                        .rev()
                        .unique_by(CommandData::get_key_clone)
                        .map(|cmd| (cmd.get_key_clone(), (cmd, create_seq_id)))
                )
            }
        };

        // 初始化wal日志
        let ver_status = Arc::new(
            VersionStatus::load_with_path(&config, &wal).await?
        );

        let compactor = Arc::new(
            Mutex::new(
                Compactor::new(
                    Arc::clone(&ver_status),
                    Arc::clone(&config),
                    ver_status.get_sst_factory(),
                )
            )
        );

        Ok(LsmStore {
            mem_table: MemTable::new(mem_map),
            ver_status,
            config,
            wal: Arc::new(wal),
            lock_file,
            vec_rev: Mutex::new(Vec::new()),
            compactor,
        })
    }

    /// 异步持久化immutable_table为SSTable
    #[inline]
    pub async fn minor_compaction(&self) -> Result<()> {
        if let Some((values, last_seq_id)) = self.mem_table.table_swap_and_sort() {
            if !values.is_empty() {
                let compactor = Arc::clone(&self.compactor);
                let gen = self.create_gen()?;
                let sender = self.live_tag().await;

                let _ignore = tokio::spawn(async move {
                    let start = Instant::now();
                    // 目前minor触发major时是同步进行的，所以此处对live_tag是在此方法体保持存活
                    if let Err(err) = compactor
                        .lock().await
                        .minor_compaction(gen, last_seq_id, values, true).await
                    {
                        error!("[LsmStore][minor_compaction][error happen]: {:?}", err);
                    }
                    sender.send(()).expect("send err!");
                    info!("[LsmStore][Compaction Drop][Time: {:?}]", start.elapsed());
                });
            }
        }
        Ok(())
    }

    fn create_gen(&self) -> Result<i64> {
        Ok(if self.is_enable_wal() {
            self.wal.switch()?
        } else {
            self.config.create_gen_lazy()
        })
    }

    /// 同步持久化immutable_table为SSTable
    #[inline]
    pub async fn minor_compaction_sync(&self, is_drop: bool) -> Result<()> {
        if let Some((values, last_seq_id)) = self.mem_table.table_swap_and_sort() {
            let gen = self.create_gen()?;

            if !values.is_empty() {
                self.compactor
                    .lock()
                    .await
                    .minor_compaction(gen, last_seq_id, values, !is_drop)
                    .await?;
            }
        }
        Ok(())
    }

    /// 同步进行SSTable基于Level的层级压缩
    #[inline]
    pub async fn major_compaction_sync(&self, level: usize) -> Result<()> {
        self.compactor
            .lock()
            .await
            .major_compaction(level, vec![])
            .await
    }

    /// 通过CommandData的引用解包并克隆出value值
    #[allow(dead_code)]
    fn value_unpack(cmd_data: &CommandData) -> Option<Vec<u8>> {
        cmd_data.get_value_clone()
    }

    #[allow(dead_code)]
    pub(crate) fn ver_status(&self) -> &Arc<VersionStatus> {
        &self.ver_status
    }

    pub(crate) fn config(&self) -> &Arc<Config> {
        &self.config
    }

    pub(crate) fn wal(&self) -> &Arc<LogLoader> {
        &self.wal
    }

    pub(crate) async fn flush_(&self, is_drop: bool) -> Result<()> {
        self.wal.flush()?;
        if !self.mem_table.is_empty() {
            self.minor_compaction_sync(is_drop).await?;
        }
        self.wait_for_compression_down().await?;

        Ok(())
    }

    /// 创建事务
    pub async fn new_trans(&self) -> Result<Transaction> {
        self.wait_for_compression_down().await?;

        Ok(Transaction::new(
            self.config(),
            self.ver_status.current().await,
            self.mem_table.inner.read(),
            self.wal()
        )?)
    }
}

#[derive(Debug)]
pub struct Config {
    /// 数据目录地址
    pub(crate) dir_path: PathBuf,
    /// WAL数量阈值
    pub(crate) wal_threshold: usize,
    /// 稀疏索引间间隔的Block(4K字节大小)数量
    pub(crate) sparse_index_interval_block_size: u64,
    /// SSTable文件大小
    pub(crate) sst_file_size: usize,
    /// Minor触发数据长度
    pub(crate) minor_threshold_with_len: usize,
    /// Major压缩触发阈值
    pub(crate) major_threshold_with_sst_size: usize,
    /// Major压缩选定文件数
    /// Major压缩时通过选定个别SSTable(即该配置项)进行下一级的SSTable选定，
    /// 并将确定范围的下一级SSTable再次对当前等级的SSTable进行范围判定，
    /// 找到最合理的上下级数据范围并压缩
    pub(crate) major_select_file_size: usize,
    /// 每级SSTable数量倍率
    pub(crate) level_sst_magnification: usize,
    /// 布隆过滤器 期望的错误概率
    pub(crate) desired_error_prob: f64,
    /// 数据库全局Position段数据缓存的数量
    /// 一个size大约为4kb(可能更少)
    /// 由于使用ShardingCache作为并行，以16为单位
    pub(crate) block_cache_size: usize,
    /// 用于缓存SSTable
    pub(crate) table_cache_size: usize,
    /// 开启wal日志写入
    /// 在开启状态时，会在SSTable文件读取失败时生效，避免数据丢失
    /// 不过在设备IO容易成为瓶颈，或使用多节点冗余写入时，建议关闭以提高写入性能
    pub(crate) wal_enable: bool,
    /// wal写入时开启异步写入
    /// 可以提高写入响应速度，但可能会导致wal日志在某种情况下并落盘慢于LSM内核而导致该条wal日志无效
    pub(crate) wal_async_put_enable: bool,
    /// gen生成器
    /// 用于SSTable以及SequenceId的生成
    gen_generator: parking_lot::Mutex<SnowflakeIdGenerator>
}

impl Config {

    pub fn new(path: impl Into<PathBuf> + Send, machine_id: i32, node_id: i32) -> Config {
        Config {
            dir_path: path.into(),
            minor_threshold_with_len: DEFAULT_MINOR_THRESHOLD_WITH_LEN,
            wal_threshold: DEFAULT_WAL_THRESHOLD,
            sparse_index_interval_block_size: DEFAULT_SPARSE_INDEX_INTERVAL_BLOCK_SIZE,
            sst_file_size: DEFAULT_SST_FILE_SIZE,
            major_threshold_with_sst_size: DEFAULT_MAJOR_THRESHOLD_WITH_SST_SIZE,
            major_select_file_size: DEFAULT_MAJOR_SELECT_FILE_SIZE,
            level_sst_magnification: DEFAULT_LEVEL_SST_MAGNIFICATION,
            desired_error_prob: DEFAULT_DESIRED_ERROR_PROB,
            block_cache_size: DEFAULT_BLOCK_CACHE_SIZE,
            table_cache_size: DEFAULT_TABLE_CACHE_SIZE,
            wal_enable: true,
            wal_async_put_enable: true,
            gen_generator: parking_lot::Mutex::new(
                SnowflakeIdGenerator::new(machine_id, node_id)
            ),
        }
    }

    #[inline]
    pub fn dir_path(mut self, dir_path: PathBuf) -> Self {
        self.dir_path = dir_path;
        self
    }

    #[inline]
    pub fn minor_threshold_with_len(mut self, minor_threshold_with_len: usize) -> Self {
        self.minor_threshold_with_len = minor_threshold_with_len;
        self
    }

    #[inline]
    pub fn wal_threshold(mut self, wal_threshold: usize) -> Self {
        self.wal_threshold = wal_threshold;
        self
    }

    #[inline]
    pub fn sparse_index_interval_block_size(mut self, sparse_index_interval_block_size: u64) -> Self {
        self.sparse_index_interval_block_size = sparse_index_interval_block_size;
        self
    }

    #[inline]
    pub fn sst_file_size(mut self, sst_file_size: usize) -> Self {
        self.sst_file_size = sst_file_size;
        self
    }

    #[inline]
    pub fn major_threshold_with_sst_size(mut self, major_threshold_with_sst_size: usize) -> Self {
        self.major_threshold_with_sst_size = major_threshold_with_sst_size;
        self
    }

    #[inline]
    pub fn major_select_file_size(mut self, major_select_file_size: usize) -> Self {
        self.major_select_file_size = major_select_file_size;
        self
    }

    #[inline]
    pub fn level_sst_magnification(mut self, level_sst_magnification: usize) -> Self {
        self.level_sst_magnification = level_sst_magnification;
        self
    }

    #[inline]
    pub fn desired_error_prob(mut self, desired_error_prob: f64) -> Self {
        self.desired_error_prob = desired_error_prob;
        self
    }

    #[inline]
    pub fn block_cache_size(mut self, cache_size: usize) -> Self {
        self.block_cache_size = cache_size;
        self
    }

    #[inline]
    pub fn table_cache_size(mut self, cache_size: usize) -> Self {
        self.table_cache_size = cache_size;
        self
    }

    #[inline]
    pub fn create_gen_lazy(&self) -> i64 {
        self.gen_generator
            .lock()
            .lazy_generate()
    }

    #[inline]
    pub fn create_gen(&self) -> i64 {
        self.gen_generator
            .lock()
            .generate()
    }

    #[inline]
    pub fn create_gen_with_size(&self, size: usize) -> Vec<i64> {
        let mut generator = self.gen_generator
            .lock();

        let mut vec_gen = Vec::with_capacity(size);
        for _ in 0..size {
            vec_gen.push(generator.lazy_generate());
        }
        vec_gen
    }

    #[inline]
    pub fn wal_enable(mut self, wal_enable: bool) -> Self {
        self.wal_enable = wal_enable;
        self
    }

    #[inline]
    pub fn wal_async_put_enable(mut self, wal_async_put_enable: bool) -> Self {
        self.wal_async_put_enable = wal_async_put_enable;
        self
    }
}

/// 日志记录，可选以Task类似的异步写数据或同步
pub(crate) async fn wal_put(wal: &Arc<LogLoader>, cmd: &CommandData, is_sync: bool) {
    let wal = Arc::clone(wal);
    if is_sync {
        wal_put_(&wal, cmd);
    } else {
        let cmd_clone = cmd.clone();
        let _ignore = tokio::spawn(async move {
            wal_put_(&wal, &cmd_clone);
        });
    }

    fn wal_put_(wal: &Arc<LogLoader>, cmd: &CommandData) {
        if let Err(err) = wal.log(&cmd) {
            error!("[LsmStore][wal_put][error happen]: {:?}", err);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;
    use itertools::Itertools;
    use tempfile::TempDir;
    use crate::kernel::lsm::lsm_kv::{Config, LsmStore};
    use crate::kernel::{KVStore, Result};

    #[test]
    fn test_lsm_major_compactor() -> Result<()> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");

        tokio_test::block_on(async move {
            let times = 5000;

            let value = b"Stray birds of summer come to my window to sing and fly away.
            And yellow leaves of autumn, which have no songs, flutter and fall
            there with a sign.";

            let config = Config::new(temp_dir.into_path(), 0, 0)
                .wal_enable(false)
                .minor_threshold_with_len(1000)
                .major_threshold_with_sst_size(4);
            let kv_store = LsmStore::open_with_config(config).await?;
            let mut vec_kv = Vec::new();

            for i in 0..times {
                let vec_u8 = bincode::serialize(&i)?;
                vec_kv.push((
                    vec_u8.clone(),
                    vec_u8.into_iter()
                        .chain(value.to_vec())
                        .collect_vec()
                ));
            }

            let start = Instant::now();
            for i in 0..times {
                kv_store.set(&vec_kv[i].0, vec_kv[i].1.clone()).await?
            }
            println!("[set_for][Time: {:?}]", start.elapsed());

            kv_store.flush().await?;

            let start = Instant::now();
            for i in 0..times {
                assert_eq!(kv_store.get(&vec_kv[i].0).await?, Some(vec_kv[i].1.clone()));
            }
            println!("[get_for][Time: {:?}]", start.elapsed());
            kv_store.flush().await?;

            Ok(())
        })
    }
}