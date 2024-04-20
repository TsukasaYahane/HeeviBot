from nonebot import on_command
from nonebot.rule import to_me
from nonebot.exception import MatcherException
from nonebot.adapters import Message
from nonebot.params import CommandArg
from nonebot.params import ArgPlainText
from nonebot.matcher import Matcher
import requests
import json
from utils.const_value import KEY, API, UNIT, LANGUAGE

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
    try :
        result = requests.get(API,params={
            'key' : KEY,
            'location' : location,
            'language' : LANGUAGE,
            'unit' : UNIT
        },timeout = 10)
        result_text = result.text
        result_dict = json.loads(result_text)

        weatherinfo = result_dict['results'][0].get('now',{})
        Day_weather = weatherinfo.get('text')
        temperature = weatherinfo.get('temperature')
        last_upadte = result_dict['results'][0].get("last_update")

        await weather.finish("{}的天气为{}℃，最后一次更新时间：{}".format(location,Day_weather+temperature,last_upadte))
        
    except MatcherException :
        raise
    except Exception as e:
        print ("获取天气失败：{}".format(e))
        pass
    
    '''try :
        await weather.finish("今天{}的天气是{}".format(location))
    except MatcherException :
            raise
    except Exception as e :
        pass   '''