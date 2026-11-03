// 浮窗窗口直接按 URL 加载本路由(不经主窗口的客户端导航),预渲染出 memo-panel.html,
// 不依赖宿主对未知路径回落 index.html。根 layout 已 ssr=false,产物是带脚本的空壳。
export const prerender = true;
