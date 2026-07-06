// app.css 的 token 全部走 light-dark(),由根元素的 color-scheme 决定取亮值还是暗值。
// 应用/设置页只需要改这一个属性,不需要碰任何 token —— 这里是唯一实现,
// 设置页手动切换主题时也要调这个函数,避免出现第二份"切主题"逻辑各写各的。
export function applyTheme(theme: string) {
  // "system" 清空内联样式,交回浏览器按 prefers-color-scheme 解析(跟随系统,默认行为);
  // "light" / "dark" 直接覆盖 color-scheme,light-dark() 立即按此取值。
  document.documentElement.style.colorScheme = theme === "system" ? "" : theme;
}
