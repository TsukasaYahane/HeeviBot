from nonebot import on_command
from nonebot.rule import to_me
from nonebot.exception import MatcherException,FinishedException
from nonebot.adapters import Message
from nonebot.params import CommandArg
from nonebot.params import ArgPlainText
from nonebot.matcher import Matcher
from .get_requests import fetchWeather

weather = on_command("天气", rule=to_me(), aliases={"weather","查天气"}, priority=10, block=True)

@weather.handle()
async def handle_function(matcher : Matcher,args:Message = CommandArg()):
    if args.extract_plain_text() :
        try :
            matcher.set_arg("location",args)
        except MatcherException :
            raise
        except Exception as e :
            pass


@weather.got("location",prompt="请输入地名")
async def got_location(location: str = ArgPlainText()):
    data = fetchWeather(location)
    if not 'results' in data :
        await weather.finish("该地区暂未支持天气查询，请重试")   
    try :
        
        weatherinfo = data['results'][0].get('now',{})
        Day_weather = weatherinfo.get('text')
        temperature = weatherinfo.get('temperature')
        last_upadte = data['results'][0].get("last_update")

        await weather.finish("{}的天气为{}℃，最后一次更新时间：{}".format(location,Day_weather+temperature,last_upadte))

    except FinishedException :
        pass
    except Exception as e:
        weather.finish("获取天气失败：{}".format(e))
    
    
    '''try :
        await weather.finish("今天{}的天气是{}".format(location))
    except MatcherException :
            raise
    except Exception as e :
        pass    '''