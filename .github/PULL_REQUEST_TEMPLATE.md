## 变更说明 / Description

<!-- 简述改了什么、为什么改 -->

## 检查清单 / Checklist

- [ ] `cargo fmt --all -- --check` 通过
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` 0 警告
- [ ] `cargo test --all-targets` 通过（或运行了 `scripts/dev-checks.sh`）
- [ ] 改动了 `scripts/` 或 `admin/` 时，对应的 Python / Node 测试通过
- [ ] 敏感数据检查通过（`python3 -B scripts/check_sensitive_data.py --staged`），未包含凭据、身份标识或聊天原文
- [ ] 涉及用户可见文本时已同步更新 `locales/en.yml` 和 `locales/zh.yml`
- [ ] 行为变化已同步到 `docs/` 中的相关文档

## 关联 Issue / Related Issue

<!-- 例如：Closes #12, Fixes #34 -->

## 截图 / 日志 / Screenshots & Logs

<!-- 仅在涉及消息格式或 UI 变更时需要 -->
