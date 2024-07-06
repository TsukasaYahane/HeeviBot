import sxtwl
import time
from .trigram import Hexa_Trigram,Change

def mon_handle(mon:str):
    mons = {
        "Jan": 1,
        "Feb": 2,
        "Mar": 3,
        "Apr": 4,
        "May": 5,
        "Jun": 6,
        "Jul": 7,
        "Aug": 8,
        "Sep": 9,
        "Oct": 10,
        "Nov": 11,
        "Dec": 12
    }

    month = mons.get(mon)
    return month

def TianDi():
    now_time = time.asctime().split()
    solar_month = mon_handle(now_time[1])
    solar_year = int(now_time[4])
    solar_day = int(now_time[2])
    hour = int(now_time[3][0:2])
    date_info = sxtwl.fromSolar(solar_year,solar_month,solar_day)

    Lunar_month = date_info.getLunarMonth()
    Lunar_year = date_info.getLunarYear()
    Lunar_day = date_info.getLunarDay()

    Dizhi_year = date_info.getYearGZ().dz + 1
    Dizhi_hour = date_info.getHourGZ(hour).dz - 1

    Trigram_up = int((Dizhi_year + Lunar_month + Lunar_day) % 8)
    if Trigram_up == 0:
        Trigram_up = 8
    Trigram_down = int((Dizhi_year + Lunar_month + Lunar_day + Dizhi_hour) % 8)
    if Trigram_down == 0:
        Trigram_down = 8
    Change_Yao = int((Dizhi_year + Lunar_month + Lunar_day + Dizhi_hour ) % 6)

    Hexa_Trigram_Orgin = Hexa_Trigram.get((Trigram_up,Trigram_down))

    if Change_Yao == 0 :
        Change_Yao = 6
    
    if Change_Yao  > 3 :
        Change_Yao -= 3
        Trigram_down = Change(Trigram_down,Change_Yao)
    else :
        Trigram_up = Change(Trigram_up,Change_Yao)

    Hexa_Trigram_Change = Hexa_Trigram.get((Trigram_up,Trigram_down))

    return Hexa_Trigram_Orgin,Hexa_Trigram_Change
