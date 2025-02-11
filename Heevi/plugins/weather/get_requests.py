import requests
import json
from .utils.const_value import  UNIT, LANGUAGE,headers
import gzip
from nonebot import logger
import matplotlib.pyplot as plt
import numpy as np
import io
from datetime import datetime
import matplotlib.dates as mdates
import matplotlib.font_manager as fm

plt.rcParams['font.sans-serif'] = ['SimHei']
plt.rcParams['axes.unicode_minus'] = False 

class weather:
    def __init__(self):
        pass

    def locationHandle(self,location):
        url = "https://geoapi.qweather.com/v2/city/lookup"

        params = {
            'location' : location,
            'lang' : 'zh-hans',
            'number' : 10
        }
        response = requests.get(url=url,headers=headers,params=params)
        data = response.json()
        if data['code'] == '200' :
            ID = data['location'][0]['id']
            return ID
        else:
            return False
        
    def weatherInTime(self,locationID:int):
        url = 'https://devapi.qweather.com/v7/weather/now'

        params = {
            'location' : locationID,
            'lang' : LANGUAGE,
            'unit' : UNIT
        }
        response = requests.get(url=url,headers=headers,params=params)
        data = response.json()
        return data
    
    def weatherForcast(self,locationID:int):
        url = 'https://devapi.qweather.com/v7/weather/7d'
        params = {
            'location' : locationID,
            'lang' : LANGUAGE,
            'unit' : UNIT
        }
        response = requests.get(url=url,headers=headers,params=params)
        data = response.json()
        return data

    def tipsHandler(self,temp):
        temp_int = int(temp)
        if temp_int <= 0 :
            tips = r'天气已经到零下了❄️非必要不出门哦'
        elif 0 < temp_int <= 12 :
            tips = r'天气比较冷☁️记得多添衣服哦' 
        elif 12< temp_int <= 20 :
            tips = r'天气比较凉爽🌀很适合出门走走哦'
        elif 20< temp_int <= 30 :
            tips = r'天气较热🌞记得多补水哦'
        elif temp_int >30 :
            tips = r'高温天气🔥注意做好防晒保护'
        return tips

    def forcastHandle(self,forcastInfo):
        totalInfo = []
                
        for data in forcastInfo['daily']:
            forecast = {
                "日期": data["fxDate"],
                "最高温度": f'{data["tempMax"]}℃',
                "最低温度": f'{data["tempMin"]}℃',
                "白天天气": data["textDay"],
                "夜间天气": data["textNight"],
                "白天风向": f'{data["windDirDay"]} {data["windScaleDay"]}级',
                "夜间风向": f'{data["windDirNight"]} {data["windScaleNight"]}级',
                "湿度": f'{data["humidity"]}%',
                "降水量": f'{data["precip"]}mm',
                "大气压": f'{data["pressure"]} hPa',
                "能见度": f'{data["vis"]} km',
                "紫外线指数": data["uvIndex"],
                "日出时间": data["sunrise"],
                "日落时间": data["sunset"],
                "月相": data["moonPhase"]
            }
            totalInfo.append(forecast)
        return totalInfo
    
    def drawForcastImg(self,data,location):
        dates = [datetime.strptime(day["日期"], "%Y-%m-%d") for day in data]
        temp_max = [int(day["最高温度"].replace("℃", "")) for day in data]
        temp_min = [int(day["最低温度"].replace("℃", "")) for day in data]
        weather_texts = [day["白天天气"] for day in data]

        plt.figure(figsize=(10,5))
        plt.plot(dates,temp_max,label='最高温度',marker='o',color = 'red',linestyle='-')
        plt.plot(dates, temp_min, label="最低温度", marker="o", color="blue", linestyle="--")

        for i, (date, text, t_max, t_min) in enumerate(zip(dates, weather_texts, temp_max, temp_min)):
            plt.text(date, t_max + 1, f"{t_max}℃", ha="center", fontsize=10, color="red")
            plt.text(date, t_min - 2, f"{t_min}℃", ha="center", fontsize=10, color="blue")
            plt.text(date, (t_max + t_min) / 2, text, ha="center", fontsize=9, color="green")

        plt.title(f"{location} 7 天天气趋势", fontsize=14)
        plt.xlabel("日期", fontsize=12)
        plt.ylabel("温度 (℃)", fontsize=12)
        plt.xticks(rotation=45)
        plt.gca().xaxis.set_major_formatter(mdates.DateFormatter("%m-%d"))
        plt.legend()
        plt.grid(True, linestyle="--", alpha=0.6)

        img_bytes = io.BytesIO()
        plt.savefig(img_bytes, format="png", dpi=100)
        img_bytes.seek(0)
        plt.close()

        return img_bytes