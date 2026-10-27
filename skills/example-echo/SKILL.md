# echo

最小技能示例：从 stdin 读 JSON，把 `text` 原样打印。

## 约定

- 参数走 stdin（一个 JSON 对象），结果走 stdout；
- 退出码 0 = 成功；
- 只能访问工作区 `/ws` 和只读的 `/skills`；
- 这个文件是给人看的，**不会进入模型上下文**。
