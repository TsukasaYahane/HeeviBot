import nonebot
from nonebot.adapters.onebot.v11 import Adapter  # 避免重复命名
from pathlib import Path

# 初始化 NoneBot
nonebot.init(custom_config1 ="config on init")

# 初始化后
config = nonebot.get_driver().config
config.custom_config1 = "change after init"
config.custom_config2 = "new config after init"
# 注册适配器
driver = nonebot.get_driver()
driver.register_adapter(Adapter)

# 加载插件
nonebot.load_builtin_plugins("echo")  # 内置插件
#nonebot.load_plugins("Heevi/plugins")  # 第三方插件
nonebot.load_plugins("Heevi/plugins")  # 本地插件

if __name__ == "__main__":
    nonebot.run()