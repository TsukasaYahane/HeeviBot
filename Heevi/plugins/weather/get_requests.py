import requests
from utils.const_value import KEY, API, UNIT, LANGUAGE

def fetchWeather(location) :
    result = requests.get(API,params={
        'key' : KEY,
        'location' : location,
        'language' : LANGUAGE,
        'unit' : UNIT
    },timeout = 10)
    return result.text
    
location = 'beijing'

print(fetchWeather(location))