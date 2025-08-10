```
dist/<name>/
├─ manifest.json            # 給 runtime/agent 用的執行描述（正式協定）
├─ persist-map.json         # { service, container_path, host_rel }
├─ appcipe.yml              # (選) 原始設定回寫，方便檢查
└─ services/
   └─ <svc>/
      └─ rootfs/           # MVP: 直接把 tar 解成檔案樹（之後換成 .squashfs）

```