; NSIS 安装钩子：右键菜单集成（桌面 / 文件夹空白处右键 → 重建图标缓存）
; 写入 HKCU 当前用户注册表，无需管理员权限；卸载时自动清理。
; ${MAINBINARYNAME} 为 tauri-bundler 注入的主程序文件名，自适应重命名。

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "注册右键菜单项（桌面 / 文件夹空白处右键） ..."
  WriteRegStr HKCU "Software\Classes\Directory\Background\shell\IconCacheRebuilder" "" "重建图标缓存"
  WriteRegStr HKCU "Software\Classes\Directory\Background\shell\IconCacheRebuilder" "Icon" "$INSTDIR\${MAINBINARYNAME}.exe,0"
  WriteRegStr HKCU "Software\Classes\Directory\Background\shell\IconCacheRebuilder\command" "" '"$INSTDIR\${MAINBINARYNAME}.exe" --rebuild'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "移除右键菜单项 ..."
  DeleteRegKey HKCU "Software\Classes\Directory\Background\shell\IconCacheRebuilder"
!macroend
