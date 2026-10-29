DSH Web 启动器（dsh-launcher）0.1.0
=======================================

做什么
  给 DSH Web 服务配一个托盘图标：启动、重启、停止，外加一个本地配置页。
  单文件、免安装，不需要 .NET / Node 运行时（前提是本机已经装了 dsh）。

首次使用
  1. 双击 dsh-launcher.exe，托盘出现鲸鱼图标。
  2. 鲸鱼是品牌蓝，右下角小圆点是状态：绿=运行中，红=已停止，橙=操作中，灰=未知。
  3. 如果端口上没有服务在跑，启动器会自己把它拉起来（就绪后图标变绿）。

托盘菜单
  状态行（点击刷新）
  打开 Web UI            带 token 的地址，直接进控制台
  打开配置页             http://127.0.0.1:3089/
  配置页开关             关掉会立即释放配置端口
  启动服务 / 重启服务 / 停止服务
  打开日志目录           %USERPROFILE%\.dsh\logs
  退出（保留服务）
  停止服务并退出

配置页
  可以改监听地址、端口、dsh profile、额外参数、就绪超时等，保存后下次启动生效；
  页面上的「启动 / 重启 / 停止」直接操作服务。

配置文件与日志
  设置：%LOCALAPPDATA%\DshLauncher\settings.json
  日志：%LOCALAPPDATA%\DshLauncher	ray.log
  服务输出：%USERPROFILE%\.dsh\logs\web-*.out.log / web-*.err.log

开机自启（可选）
  Win+R → 输入 shell:startup → 回车，把 dsh-launcher.exe 的快捷方式放进去。

命令行
  dsh-launcher.exe -Action Status|Url|Start|Restart|Stop|Admin|Icon
  可用参数：-Port 3080   -Host 127.0.0.1   -HeapMb 0
            -ReadyTimeout 90   -StopTimeout 15
            -NodeExe <路径>   -EntryScript <路径>   -LogDirectory <目录>
            -NoAutoStart（启动时不要自动拉起服务）
            -AdminPort 3089   -NoAdmin
  例：dsh-launcher.exe -Action Status -Port 3080

卸载
  退出托盘，删掉 exe，再删掉 %LOCALAPPDATA%\DshLauncher 即可。

依赖
  系统里需要有 node + @deepseek-ai/dsh；启动器会自动在
  %USERPROFILE%\.sdk\cache
ode\*\*
ode_modules\@deepseek-ai\dsh 下找安装位置。
