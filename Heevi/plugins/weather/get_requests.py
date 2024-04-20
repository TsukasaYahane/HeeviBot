import requests
from utils.const_value import KEY, API, UNIT, LANGUAGE

async def fetchWeather(location) :
    try :
        result = requests.get(API,params={
            'key' : KEY,
            'location' : location,
            'language' : LANGUAGE,
            'unit' : UNIT
        },timeout = 10)
        return result.text
    except Exception as e:
        return "获取天气失败：{}".format(e)
    
location = 'beijing'

print(fetchWeather(location))