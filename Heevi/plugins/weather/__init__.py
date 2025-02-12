from nonebot import on_command,logger
from nonebot.rule import to_me
from nonebot.exception import MatcherException,FinishedException
from nonebot.adapters.onebot.v11 import Message,Bot,GroupMessageEvent,PrivateMessageEvent,MessageSegment
from nonebot.params import CommandArg,ArgPlainText
from nonebot.matcher import Matcher
from .get_requests import weather
from typing import Union

weatherapi = weather()
weatherInTime = on_command("实时天气", rule=to_me(),
                            priority=10, block=True)
weatherForcast = on_command("天气预报",aliases={'天气'}, 
                            rule=to_me(), priority=10, block=True)

@weatherInTime.handle()
async def getLocation(matcher:Matcher,arg:Message = CommandArg()):
    if arg.extract_plain_text():
        matcher.set_arg("location",arg)

@weatherInTime.got("location",prompt="想看哪个地方的天气呢")
async def getWeatherInTime(location=ArgPlainText()):
    try :
        areaID = weatherapi.locationHandle(location)
        if not areaID :
            logger.error('获取城市ID失败')
            await weatherInTime.finish('哎呀，好像不存在这个地方呢')
        else :
            logger.success('获取城市ID成功')
            weatherData = weatherapi.weatherInTime(areaID)
            if weatherData['code'] != '200' :
                logger.error('获取实时天气失败')
                await weatherInTime.finish('哎呀，好像获取失败了(｀・ω・´)绝对不是Heevi的错哦')
            else:
                logger.success('获取实时天气成功')
                info = weatherData['now']
                tips = weatherapi.tipsHandler(info['temp'])
                await weatherInTime.finish(
                    MessageSegment.text(f'{location}现在的天气是{info['text']}') + MessageSegment.text(r'( • ̀ω•́ )✧') + MessageSegment.text('\n')
                    + MessageSegment.text(f'温度为{info['temp']}℃,') + MessageSegment.text(f'体感温度为{info['feelsLike']}℃,') +MessageSegment.text(f'{tips}\n')
                    + MessageSegment.text(f'目前风向是{info['windDir']}') + MessageSegment.text(r'ヾ(ｏ･ω･)ﾉ' ) + MessageSegment.text(f'风速等级为{info['windScale']}\n')
                    + MessageSegment.text(f'过去一小时降雨量为{info['precip']}mm') + MessageSegment.text(r'ヾ(=･ω･=)o' ) + MessageSegment.text(f'相对湿度为{info['humidity']}%\n')
                    + MessageSegment.text(f'大气压强为{info['pressure']}hPa') + MessageSegment.text(r'( • ̀ω•́ )✧' ) + MessageSegment.text(f'能见度约为{info['vis']}km\n')
                    + MessageSegment.text(f'以上信息最新更新时间为{weatherData['updateTime']}')
                )
    except Exception as e:
        if isinstance(e, FinishedException):
            pass
        else :
            logger.error(f'发生异常，异常为{e}')
            await weatherInTime.finish('发生异常，请查看后台或联系管理员')

@weatherForcast.handle()            
async def getLocation(matcher:Matcher,arg:Message = CommandArg()):
    if arg.extract_plain_text():
        matcher.set_arg("location",arg)

@weatherForcast.got('location',prompt='想知道哪个地区的天气预报呢')
async def Forcast(location=ArgPlainText()):
    try :
        areaID = weatherapi.locationHandle(location)
        if not areaID :
            logger.error('获取城市ID失败')
            await weatherForcast.finish('哎呀，好像不存在这个地方呢')
        else :
            logger.success('获取城市ID成功')
            forcastInfo = weatherapi.weatherForcast(areaID)
            if forcastInfo['code'] != '200' :
                logger.error('获取预报数据失败')
                await weatherForcast.finish('哎呀，好像获取失败了(｀・ω・´)绝对不是Heevi的错哦')
            else:
                logger.success('获取预报数据成功')
                totalInfo = weatherapi.forcastHandle(forcastInfo)
                forcastImg = weatherapi.drawForcastImg(totalInfo,location)
                if forcastImg:
                    logger.success('成功生成天气预报图')
                    await weatherForcast.finish(MessageSegment.image(forcastImg))
                else :
                    logger.error('生成天气预报图失败')
                    await weatherForcast.finish('生成天气预报图失败，请联系管理员或查看后台')

    except Exception as e:
        if isinstance(e, FinishedException):
            pass
        else :
            logger.error(f'发生异常，异常为{e}')
            await weatherInTime.finish('发生异常，请查看后台或联系管理员')
                




                

