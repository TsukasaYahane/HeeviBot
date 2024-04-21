import requests
import json
from .utils.const_value import KEY, API, UNIT, LANGUAGE

def fetchWeather(location) :

    result = requests.get(API,params={
        'key' : KEY,
        'location' : location,
        'language' : LANGUAGE,
        'unit' : UNIT
    },timeout = 10)
    result_dict = result.json()
    return result_dict

